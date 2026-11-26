//! Lanzador gráfico de tela (GTK4).
//!
//! Permite elegir la presentación y las notas, comprobar con una vista previa lado a lado que
//! cada nota corresponde a su diapositiva, ajustar opciones y presentar. Los ajustes se
//! recuerdan por presentación. Al pulsar «Presentar» este proceso se reemplaza por `tela`
//! (`exec`), así que durante la presentación no queda nada de GTK en memoria.

use std::cell::RefCell;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use tela::render::Ink;
use tela::settings::{Settings, Store};
use tela::{args, convert};

const APP_ID: &str = "app.tela.Lanzador";
const INKS: [Ink; 3] = [Ink::Dark, Ink::Light, Ink::Off];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Slides,
    Notes,
}

/// Un documento elegido: la ruta original (PDF o PPT) y el PDF ya abierto para la vista previa.
struct Doc {
    path: PathBuf,
    pdf: Option<poppler::Document>,
    pages: u32,
}

#[derive(Default)]
struct State {
    slides: Option<Doc>,
    notes: Option<Doc>,
    page: u32,
    store: Store,
    /// Generación de carga por ranura: descarta conversiones que terminen tarde.
    generation: [u32; 2],
    converting: u32,
}

struct Gui {
    window: gtk::ApplicationWindow,
    slides_label: gtk::Label,
    notes_label: gtk::Label,
    clear_notes: gtk::Button,
    slide_area: gtk::DrawingArea,
    notes_area: gtk::DrawingArea,
    slide_caption: gtk::Label,
    notes_caption: gtk::Label,
    page_label: gtk::Label,
    prev: gtk::Button,
    next: gtk::Button,
    offset: gtk::SpinButton,
    opacity: gtk::Scale,
    ink: gtk::DropDown,
    monitor: gtk::DropDown,
    monitors: Vec<Option<String>>,
    windowed: gtk::CheckButton,
    spinner: gtk::Spinner,
    status: gtk::Label,
    present: gtk::Button,
    state: RefCell<State>,
}

fn main() -> glib::ExitCode {
    // `tela-gui <archivo>` abre el lanzador con esa presentación ya elegida.
    let initial = std::env::args_os().nth(1).map(PathBuf::from);
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let gui = build(app);
        if let Some(p) = &initial {
            set_slides(&gui, p.clone());
        }
        refresh(&gui);
        gui.window.present();
    });
    app.run_with_args::<&str>(&[])
}

// ---------------------------------------------------------------------------------------------
// Construcción de la ventana
// ---------------------------------------------------------------------------------------------

fn build(app: &gtk::Application) -> Rc<Gui> {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("tela")
        .default_width(960)
        .default_height(680)
        .build();

    let present = gtk::Button::with_label("Presentar");
    present.add_css_class("suggested-action");
    present.set_tooltip_text(Some("Abrir la presentación con estas notas (Ctrl+Intro)"));
    let header = gtk::HeaderBar::new();
    header.pack_end(&present);
    window.set_titlebar(Some(&header));

    // --- Archivos ---
    let files = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .build();
    let slides_label = file_label("Ninguna presentación elegida");
    let notes_label = file_label("Sin notas");
    let pick_slides = gtk::Button::with_label("Elegir…");
    let pick_notes = gtk::Button::with_label("Elegir…");
    let clear_notes = gtk::Button::from_icon_name("edit-clear-symbolic");
    clear_notes.set_tooltip_text(Some("Quitar las notas"));
    files.attach(&row_title("Presentación"), 0, 0, 1, 1);
    files.attach(&slides_label, 1, 0, 1, 1);
    files.attach(&pick_slides, 2, 0, 2, 1);
    files.attach(&row_title("Notas"), 0, 1, 1, 1);
    files.attach(&notes_label, 1, 1, 1, 1);
    files.attach(&pick_notes, 2, 1, 1, 1);
    files.attach(&clear_notes, 3, 1, 1, 1);

    // --- Vista previa lado a lado ---
    let slide_area = preview_area();
    let notes_area = preview_area();
    let slide_caption = caption();
    let notes_caption = caption();
    let previews = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    previews.set_homogeneous(true);
    previews.append(&labeled(&slide_area, &slide_caption));
    previews.append(&labeled(&notes_area, &notes_caption));

    let prev = gtk::Button::from_icon_name("go-previous-symbolic");
    let next = gtk::Button::from_icon_name("go-next-symbolic");
    prev.set_tooltip_text(Some("Diapositiva anterior (RePág)"));
    next.set_tooltip_text(Some("Diapositiva siguiente (AvPág)"));
    let page_label = gtk::Label::new(None);
    page_label.set_width_chars(18);
    let nav = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    nav.set_halign(gtk::Align::Center);
    nav.append(&prev);
    nav.append(&page_label);
    nav.append(&next);

    // --- Opciones ---
    let options = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .build();
    let offset = gtk::SpinButton::with_range(-50.0, 50.0, 1.0);
    offset.set_tooltip_text(Some(
        "Cuántas páginas van corridas las notas. Usa +1 si el PDF de notas tiene una portada extra.",
    ));
    let opacity = gtk::Scale::with_range(gtk::Orientation::Horizontal, 5.0, 100.0, 5.0);
    opacity.set_draw_value(true);
    opacity.set_value_pos(gtk::PositionType::Right);
    opacity.set_hexpand(true);
    opacity.set_format_value_func(|_, v| format!("{v:.0} %"));
    let ink = gtk::DropDown::from_strings(&[
        "Oscura — el fondo blanco se vuelve transparente",
        "Clara — para diapositivas oscuras",
        "Página completa — con su fondo",
    ]);
    let (monitor, monitors) = monitor_dropdown();
    let windowed = gtk::CheckButton::with_label("Empezar en ventana (no a pantalla completa)");

    options.attach(&row_title("Desfase de notas"), 0, 0, 1, 1);
    options.attach(&offset, 1, 0, 1, 1);
    options.attach(&row_title("Opacidad"), 2, 0, 1, 1);
    options.attach(&opacity, 3, 0, 1, 1);
    options.attach(&row_title("Tinta"), 0, 1, 1, 1);
    options.attach(&ink, 1, 1, 3, 1);
    options.attach(&row_title("Monitor"), 0, 2, 1, 1);
    options.attach(&monitor, 1, 2, 3, 1);
    options.attach(&windowed, 1, 3, 3, 1);

    // --- Estado ---
    let spinner = gtk::Spinner::new();
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.set_hexpand(true);
    let tip = gtk::Label::new(Some(
        "En Meet o Zoom comparte la ventana «tela — …», no la pantalla completa: así las notas \
         solo las ves tú.",
    ));
    tip.add_css_class("dim-label");
    tip.set_wrap(true);
    tip.set_xalign(0.0);
    let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    status_row.append(&spinner);
    status_row.append(&status);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 14);
    for m in [
        gtk::Widget::set_margin_top,
        gtk::Widget::set_margin_bottom,
        gtk::Widget::set_margin_start,
        gtk::Widget::set_margin_end,
    ] {
        m(root.upcast_ref(), 18);
    }
    root.append(&files);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&previews);
    root.append(&nav);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&options);
    root.append(&status_row);
    root.append(&tip);
    window.set_child(Some(&root));

    let gui = Rc::new(Gui {
        window,
        slides_label,
        notes_label,
        clear_notes,
        slide_area,
        notes_area,
        slide_caption,
        notes_caption,
        page_label,
        prev,
        next,
        offset,
        opacity,
        ink,
        monitor,
        monitors,
        windowed,
        spinner,
        status,
        present,
        state: RefCell::new(State {
            store: Store::load(),
            ..Default::default()
        }),
    });
    apply_settings(&gui, &Settings::default());
    connect(&gui, &pick_slides, &pick_notes);
    gui
}

fn connect(gui: &Rc<Gui>, pick_slides: &gtk::Button, pick_notes: &gtk::Button) {
    let g = gui.clone();
    pick_slides.connect_clicked(move |_| choose_file(&g, Slot::Slides));
    let g = gui.clone();
    pick_notes.connect_clicked(move |_| choose_file(&g, Slot::Notes));
    let g = gui.clone();
    gui.clear_notes.connect_clicked(move |_| {
        g.state.borrow_mut().notes = None;
        refresh(&g);
    });

    let g = gui.clone();
    gui.prev.connect_clicked(move |_| step(&g, -1));
    let g = gui.clone();
    gui.next.connect_clicked(move |_| step(&g, 1));
    let g = gui.clone();
    gui.offset.connect_value_changed(move |_| refresh(&g));
    let g = gui.clone();
    gui.present.connect_clicked(move |_| present(&g));

    for (area, slot) in [
        (&gui.slide_area, Slot::Slides),
        (&gui.notes_area, Slot::Notes),
    ] {
        let g = Rc::downgrade(gui);
        area.set_draw_func(move |_, cr, w, h| {
            if let Some(g) = g.upgrade() {
                draw_preview(&g, slot, cr, w, h);
            }
        });
        // Arrastrar un archivo sobre la vista previa lo elige para esa ranura.
        let drop = gtk::DropTarget::new(gio::File::static_type(), gdk::DragAction::COPY);
        let g = gui.clone();
        drop.connect_drop(move |_, value, _, _| {
            match value.get::<gio::File>().ok().and_then(|f| f.path()) {
                Some(path) => {
                    set_file(&g, slot, path);
                    true
                }
                None => false,
            }
        });
        area.add_controller(drop);
    }

    // Atajos: RePág/AvPág recorren la vista previa; Ctrl+Intro presenta.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let g = gui.clone();
    keys.connect_key_pressed(move |_, key, _, mods| match key {
        gdk::Key::Page_Down => {
            step(&g, 1);
            glib::Propagation::Stop
        }
        gdk::Key::Page_Up => {
            step(&g, -1);
            glib::Propagation::Stop
        }
        gdk::Key::Return | gdk::Key::KP_Enter if mods.contains(gdk::ModifierType::CONTROL_MASK) => {
            present(&g);
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
    gui.window.add_controller(keys);
}

// ---------------------------------------------------------------------------------------------
// Widgets auxiliares
// ---------------------------------------------------------------------------------------------

fn row_title(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.add_css_class("heading");
    l
}

fn file_label(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.set_hexpand(true);
    l.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    l.set_selectable(true);
    l
}

fn caption() -> gtk::Label {
    let l = gtk::Label::new(None);
    l.add_css_class("dim-label");
    l
}

fn preview_area() -> gtk::DrawingArea {
    let a = gtk::DrawingArea::new();
    a.set_content_height(260);
    a.set_hexpand(true);
    a.set_vexpand(true);
    a
}

fn labeled(area: &gtk::DrawingArea, caption: &gtk::Label) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let frame = gtk::Frame::new(None);
    frame.set_child(Some(area));
    b.append(&frame);
    b.append(caption);
    b
}

/// Lista de monitores con su nombre de conector (el mismo que usa `hyprctl monitors`).
fn monitor_dropdown() -> (gtk::DropDown, Vec<Option<String>>) {
    let mut labels = vec!["Automático (el monitor donde se abra la ventana)".to_string()];
    let mut names = vec![None];
    if let Some(display) = gdk::Display::default() {
        let list = display.monitors();
        for i in 0..list.n_items() {
            let Some(m) = list.item(i).and_downcast::<gdk::Monitor>() else {
                continue;
            };
            let Some(name) = m.connector().map(|c| c.to_string()) else {
                continue;
            };
            let g = m.geometry();
            let model = [m.manufacturer(), m.model()]
                .into_iter()
                .flatten()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            let mut label = format!("{name} — {}×{}", g.width(), g.height());
            if !model.is_empty() {
                label.push_str(&format!(" — {model}"));
            }
            labels.push(label);
            names.push(Some(name));
        }
    }
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    (gtk::DropDown::from_strings(&refs), names)
}

// ---------------------------------------------------------------------------------------------
// Lógica
// ---------------------------------------------------------------------------------------------

fn choose_file(gui: &Rc<Gui>, slot: Slot) {
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    let all = gtk::FileFilter::new();
    all.set_name(Some("PDF y presentaciones"));
    for ext in ["pdf", "ppt", "pptx", "pps", "ppsx", "pptm", "odp", "key"] {
        all.add_suffix(ext);
    }
    filters.append(&all);
    let pdf = gtk::FileFilter::new();
    pdf.set_name(Some("Solo PDF"));
    pdf.add_suffix("pdf");
    filters.append(&pdf);

    let title = match slot {
        Slot::Slides => "Elegir la presentación",
        Slot::Notes => "Elegir el archivo de notas",
    };
    let dialog = gtk::FileDialog::builder()
        .title(title)
        .modal(true)
        .filters(&filters)
        .build();
    let current = match slot {
        Slot::Slides => gui.state.borrow().slides.as_ref().map(|d| d.path.clone()),
        Slot::Notes => gui.state.borrow().notes.as_ref().map(|d| d.path.clone()),
    }
    .or_else(|| gui.state.borrow().slides.as_ref().map(|d| d.path.clone()));
    if let Some(dir) = current.as_deref().and_then(Path::parent) {
        dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
    }

    let g = gui.clone();
    dialog.open(Some(&gui.window), gio::Cancellable::NONE, move |res| {
        if let Some(path) = res.ok().and_then(|f| f.path()) {
            set_file(&g, slot, path);
        }
    });
}

fn set_file(gui: &Rc<Gui>, slot: Slot, path: PathBuf) {
    match slot {
        Slot::Slides => set_slides(gui, path),
        Slot::Notes => load_doc(gui, Slot::Notes, path),
    }
}

/// Elige la presentación y recupera sus ajustes guardados (o busca sus notas junto a ella).
fn set_slides(gui: &Rc<Gui>, path: PathBuf) {
    let path = path.canonicalize().unwrap_or(path);
    let saved = gui.state.borrow().store.get(&path).cloned();
    let settings = saved.clone().unwrap_or_default();
    apply_settings(gui, &settings);

    let notes = match &saved {
        Some(s) => s.notes.clone().filter(|n| n.is_file()),
        None => args::find_notes(&path),
    };
    {
        let mut st = gui.state.borrow_mut();
        st.page = 0;
        st.notes = None;
    }
    load_doc(gui, Slot::Slides, path);
    if let Some(n) = notes {
        load_doc(gui, Slot::Notes, n);
    }
    set_status(
        gui,
        if saved.is_some() {
            "Ajustes recuperados de la última vez."
        } else {
            ""
        },
    );
}

/// Carga un documento: los PDF se abren al instante; los PowerPoint se convierten en segundo
/// plano con LibreOffice (la conversión queda en caché para `tela`).
fn load_doc(gui: &Rc<Gui>, slot: Slot, path: PathBuf) {
    let path = path.canonicalize().unwrap_or(path);
    let generation = {
        let mut st = gui.state.borrow_mut();
        let gen_slot = &mut st.generation[slot as usize];
        *gen_slot += 1;
        let generation = *gen_slot;
        let doc = Doc {
            path: path.clone(),
            pdf: None,
            pages: 0,
        };
        match slot {
            Slot::Slides => st.slides = Some(doc),
            Slot::Notes => st.notes = Some(doc),
        }
        generation
    };
    refresh(gui);

    let is_pdf = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    if is_pdf {
        finish_load(gui, slot, generation, convert::ensure_pdf(&path));
        return;
    }

    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    gui.state.borrow_mut().converting += 1;
    gui.spinner.start();
    set_status(
        gui,
        &format!("Convirtiendo «{name}» con LibreOffice (solo la primera vez)…"),
    );
    let g = gui.clone();
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || convert::ensure_pdf(&path))
            .await
            .unwrap_or_else(|_| Err("la conversión falló inesperadamente".into()));
        let pending = {
            let mut st = g.state.borrow_mut();
            st.converting -= 1;
            st.converting
        };
        if pending == 0 {
            g.spinner.stop();
        }
        if result.is_ok() {
            set_status(&g, "");
        }
        finish_load(&g, slot, generation, result);
    });
}

fn finish_load(gui: &Rc<Gui>, slot: Slot, generation: u32, pdf: Result<PathBuf, String>) {
    if gui.state.borrow().generation[slot as usize] != generation {
        return; // el usuario eligió otro archivo mientras tanto
    }
    let opened = pdf.and_then(|p| {
        let uri = glib::filename_to_uri(&p, None).map_err(|e| e.to_string())?;
        poppler::Document::from_file(&uri, None).map_err(|e| e.to_string())
    });
    {
        let mut st = gui.state.borrow_mut();
        let doc = match slot {
            Slot::Slides => st.slides.as_mut(),
            Slot::Notes => st.notes.as_mut(),
        };
        match (doc, &opened) {
            (Some(d), Ok(pdf)) => {
                d.pages = pdf.n_pages().max(0) as u32;
                d.pdf = Some(pdf.clone());
            }
            (Some(_), Err(_)) => match slot {
                Slot::Slides => st.slides = None,
                Slot::Notes => st.notes = None,
            },
            (None, _) => {}
        }
        let pages = st.slides.as_ref().map_or(0, |d| d.pages);
        st.page = st.page.min(pages.saturating_sub(1));
    }
    if let Err(e) = opened {
        set_status(gui, &format!("No se pudo abrir el archivo: {e}"));
    }
    refresh(gui);
}

fn step(gui: &Rc<Gui>, delta: i64) {
    {
        let mut st = gui.state.borrow_mut();
        let pages = st.slides.as_ref().map_or(0, |d| d.pages) as i64;
        if pages == 0 {
            return;
        }
        st.page = (st.page as i64 + delta).clamp(0, pages - 1) as u32;
    }
    refresh(gui);
}

/// Página de notas que acompaña a la diapositiva actual (si existe).
fn notes_page(st: &State, offset: i32) -> Option<u32> {
    let notes = st.notes.as_ref()?;
    let p = st.page as i64 + offset as i64;
    (0..notes.pages as i64).contains(&p).then_some(p as u32)
}

fn refresh(gui: &Gui) {
    let st = gui.state.borrow();
    let offset = gui.offset.value_as_int();
    let name = |d: &Doc| {
        d.path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };

    match &st.slides {
        Some(d) => {
            gui.slides_label.set_text(&name(d));
            gui.slides_label.set_tooltip_text(d.path.to_str());
        }
        None => gui.slides_label.set_text("Ninguna presentación elegida"),
    }
    match &st.notes {
        Some(d) => {
            gui.notes_label.set_text(&name(d));
            gui.notes_label.set_tooltip_text(d.path.to_str());
        }
        None => {
            gui.notes_label
                .set_text("Sin notas (se presentará solo con el cronómetro)");
            gui.notes_label.set_tooltip_text(None);
        }
    }
    gui.clear_notes.set_sensitive(st.notes.is_some());

    let pages = st.slides.as_ref().map_or(0, |d| d.pages);
    if pages > 0 {
        gui.page_label
            .set_text(&format!("Diapositiva {} de {pages}", st.page + 1));
        gui.slide_caption
            .set_text(&format!("Lo que ve la audiencia · {}/{pages}", st.page + 1));
    } else {
        gui.page_label.set_text("");
        gui.slide_caption
            .set_text("Elige o arrastra aquí la presentación");
    }
    gui.prev.set_sensitive(pages > 0 && st.page > 0);
    gui.next.set_sensitive(pages > 0 && st.page + 1 < pages);

    let notes_text = match (&st.notes, notes_page(&st, offset)) {
        (None, _) => "Elige o arrastra aquí el PDF de notas".to_string(),
        (Some(d), _) if d.pdf.is_none() => "Cargando…".to_string(),
        (Some(d), Some(p)) => format!("Tu nota · página {}/{}", p + 1, d.pages),
        (Some(_), None) => "Sin nota para esta diapositiva".to_string(),
    };
    gui.notes_caption.set_text(&notes_text);

    let ready = st.slides.as_ref().is_some_and(|d| d.pdf.is_some()) && st.converting == 0;
    gui.present.set_sensitive(ready);
    drop(st);
    gui.slide_area.queue_draw();
    gui.notes_area.queue_draw();
}

fn draw_preview(gui: &Gui, slot: Slot, cr: &gtk::cairo::Context, w: i32, h: i32) {
    let (w, h) = (w as f64, h as f64);
    cr.set_source_rgb(0.12, 0.12, 0.14);
    let _ = cr.paint();

    let st = gui.state.borrow();
    let page = match slot {
        Slot::Slides => st
            .slides
            .as_ref()
            .and_then(|d| d.pdf.as_ref()?.page(st.page as i32)),
        Slot::Notes => notes_page(&st, gui.offset.value_as_int())
            .and_then(|p| st.notes.as_ref()?.pdf.as_ref()?.page(p as i32)),
    };
    let Some(page) = page else {
        // Marco punteado: zona donde elegir o soltar un archivo.
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.18);
        cr.set_line_width(2.0);
        cr.set_dash(&[8.0, 6.0], 0.0);
        cr.rectangle(12.0, 12.0, w - 24.0, h - 24.0);
        let _ = cr.stroke();
        return;
    };

    let (pw, ph) = page.size();
    let margin = 12.0;
    let s = ((w - 2.0 * margin) / pw)
        .min((h - 2.0 * margin) / ph)
        .max(0.01);
    let (dx, dy) = (((w - pw * s) / 2.0).round(), ((h - ph * s) / 2.0).round());
    cr.save().ok();
    cr.translate(dx, dy);
    cr.scale(s, s);
    cr.rectangle(0.0, 0.0, pw, ph);
    cr.set_source_rgb(1.0, 1.0, 1.0);
    let _ = cr.fill();
    page.render(cr);
    cr.restore().ok();
}

fn apply_settings(gui: &Gui, s: &Settings) {
    gui.offset.set_value(s.offset as f64);
    gui.opacity.set_value(s.opacity as f64);
    let ink = INKS.iter().position(|i| *i == s.ink).unwrap_or(0);
    gui.ink.set_selected(ink as u32);
    let monitor = gui
        .monitors
        .iter()
        .position(|m| *m == s.monitor)
        .unwrap_or(0);
    gui.monitor.set_selected(monitor as u32);
    gui.windowed.set_active(s.windowed);
}

fn read_settings(gui: &Gui) -> Settings {
    let st = gui.state.borrow();
    Settings {
        notes: st.notes.as_ref().map(|d| d.path.clone()),
        offset: gui.offset.value_as_int(),
        opacity: gui.opacity.value().round().clamp(5.0, 100.0) as u8,
        ink: INKS
            .get(gui.ink.selected() as usize)
            .copied()
            .unwrap_or(Ink::Dark),
        monitor: gui
            .monitors
            .get(gui.monitor.selected() as usize)
            .cloned()
            .flatten(),
        windowed: gui.windowed.is_active(),
    }
}

fn set_status(gui: &Gui, text: &str) {
    gui.status.set_text(text);
}

/// Guarda los ajustes y reemplaza este proceso por `tela` con los argumentos elegidos.
fn present(gui: &Rc<Gui>) {
    if !gui.present.is_sensitive() {
        return;
    }
    let Some(slides) = gui.state.borrow().slides.as_ref().map(|d| d.path.clone()) else {
        return;
    };
    let settings = read_settings(gui);
    {
        let mut st = gui.state.borrow_mut();
        st.store.put(&slides, settings.clone());
        if let Err(e) = st.store.save() {
            eprintln!("tela-gui: no se pudieron guardar los ajustes: {e}");
        }
    }

    let tela = std::env::current_exe()
        .ok()
        .and_then(|p| Some(p.parent()?.join("tela")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("tela"));
    let err = Command::new(&tela).args(settings.to_args(&slides)).exec();
    set_status(
        gui,
        &format!("No se pudo iniciar tela ({}): {err}", tela.display()),
    );
}
