//! Cliente Wayland: una ventana normal con la presentación (la que se comparte en Meet/Zoom) y dos
//! superficies `wlr-layer-shell` en la capa Overlay —las notas y el panel del cronómetro— que
//! no reciben foco ni clics y no forman parte de la ventana, por lo que no salen al compartirla.
//!
//! Todo se dibuja por software en búferes `wl_shm`; no hay bucle de render: solo se dibuja cuando
//! algo cambia (tecla, render terminado, reconfiguración o tic del reloj del panel).

use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_registry,
    dispatch2::Dispatch2,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop,
            channel::{self, Channel},
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
        client::{
            Connection, QueueHandle,
            globals::registry_queue_init,
            protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use wayland_protocols::wp::{
    alpha_modifier::v1::client::{
        wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1,
        wp_alpha_modifier_v1::WpAlphaModifierV1,
    },
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

use crate::args::Args;
use crate::render::{self, Image, Info, Ink, Key, Kind, Renderer};

/// Tamaño lógico del panel del cronómetro.
const HUD_W: u32 = 420;
const HUD_H: u32 = 38;
const HUD_MARGIN: i32 = 16;
/// Páginas rasterizadas por superficie: actual, siguiente (precargada) y la anterior.
const CACHE_PER_SURFACE: usize = 3;
const LARGE_PAGE: usize = 20 << 20;

fn canvas_bytes(w: u32, h: u32) -> usize {
    w as usize * h as usize * 4
}
const FLASH_FOR: Duration = Duration::from_millis(1600);
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Which {
    Slide,
    Notes,
    Hud,
}

// ---------------------------------------------------------------------------------------------
// Escalado (fraccional vía wp_fractional_scale + wp_viewporter, o entero como respaldo)
// ---------------------------------------------------------------------------------------------

struct Scale {
    logical: (u32, u32),
    /// Escala en 120-avos (120 = 1.0), como en wp_fractional_scale.
    s120: u32,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    /// Falso hasta recibir la escala preferida (evita renderizar a un tamaño que se descartará).
    known: bool,
}

impl Scale {
    fn new(
        surface: &wl_surface::WlSurface,
        which: Which,
        viewporter: Option<&WpViewporter>,
        fractional: Option<&WpFractionalScaleManagerV1>,
        qh: &QueueHandle<App>,
    ) -> Self {
        let viewport = viewporter.map(|v| v.get_viewport(surface, qh, NoEvents));
        let fractional = fractional
            .filter(|_| viewport.is_some())
            .map(|m| m.get_fractional_scale(surface, qh, FracData(which)));
        let known = fractional.is_none();
        Scale {
            logical: (0, 0),
            s120: 120,
            viewport,
            fractional,
            known,
        }
    }

    fn physical(&self) -> (u32, u32) {
        // Redondeo "a la mitad, lejos de cero", como pide el protocolo.
        let f = |l: u32| ((l as u64 * self.s120 as u64 + 60) / 120).max(1) as u32;
        (f(self.logical.0), f(self.logical.1))
    }

    fn apply(&self, surface: &wl_surface::WlSurface) {
        match &self.viewport {
            Some(vp) => vp.set_destination(self.logical.0 as i32, self.logical.1 as i32),
            None => surface.set_buffer_scale((self.s120 / 120).max(1) as i32),
        }
    }

    fn destroy(&mut self) {
        if let Some(f) = self.fractional.take() {
            f.destroy();
        }
        if let Some(v) = self.viewport.take() {
            v.destroy();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Superficies
// ---------------------------------------------------------------------------------------------

/// Búferes `wl_shm` de una superficie. Las páginas rasterizadas se guardan ya en memoria
/// compartida con el compositor: cambiar de página es adjuntar un búfer existente, sin copiar.
struct Paint {
    pool: SlotPool,
    cache: Vec<(Key, Buffer)>,
    /// Búfer de relleno (negro, transparente o el panel); con viewporter mide 1×1 px.
    solid: Option<Buffer>,
    has_content: bool,
}

impl Paint {
    fn new(shm: &Shm) -> Option<Self> {
        let pool = SlotPool::new(4096, shm).ok()?;
        Some(Paint {
            pool,
            cache: Vec::new(),
            solid: None,
            has_content: false,
        })
    }

    fn has(&self, key: Key) -> bool {
        self.cache.iter().any(|(k, _)| *k == key)
    }

    /// Copia una página renderizada a un búfer nuevo del pool y la guarda en caché.
    fn store(&mut self, img: &Image, format: wl_shm::Format) {
        if self.has(img.key) {
            return;
        }
        let (w, h) = (img.key.width, img.key.height);
        let Ok((buffer, canvas)) =
            self.pool
                .create_buffer(w as i32, h as i32, w as i32 * 4, format)
        else {
            eprintln!("tela: no se pudo reservar memoria para {w}x{h}");
            return;
        };
        let row = w as usize * 4;
        let src = img.bytes();
        if img.stride == row {
            canvas.copy_from_slice(&src[..canvas.len()]);
        } else {
            for (y, dst) in canvas.chunks_exact_mut(row).enumerate() {
                dst.copy_from_slice(&src[y * img.stride..][..row]);
            }
        }
        self.cache.insert(0, (img.key, buffer));
        // En pantallas grandes (4K: ~33 MB por página) se guardan solo actual y siguiente.
        let cap = if canvas_bytes(w, h) > LARGE_PAGE {
            2
        } else {
            CACHE_PER_SURFACE
        };
        // Un búfer descartado que el compositor aún use se libera cuando este lo suelte.
        self.cache.truncate(cap);
    }

    /// Muestra la página `key` si está en caché.
    fn show(&mut self, key: Key, surface: &wl_surface::WlSurface, scale: &Scale) -> bool {
        let Some(i) = self.cache.iter().position(|(k, _)| *k == key) else {
            return false;
        };
        let entry = self.cache.remove(i);
        self.cache.insert(0, entry);
        let buffer = &self.cache[0].1;
        // Si el compositor aún no soltó este búfer (contenido inmutable), se puede volver a
        // adjuntar tal cual.
        if buffer.attach_to(surface).is_err() {
            surface.attach(Some(buffer.wl_buffer()), 0, 0);
        }
        scale.apply(surface);
        surface.damage_buffer(0, 0, key.width as i32, key.height as i32);
        surface.commit();
        self.solid = None;
        self.has_content = true;
        true
    }

    /// Muestra un color sólido (BGRA premultiplicado). Con viewporter basta un búfer de 1×1.
    fn show_solid(
        &mut self,
        surface: &wl_surface::WlSurface,
        scale: &Scale,
        format: wl_shm::Format,
        px: [u8; 4],
    ) {
        let size = if scale.viewport.is_some() {
            Some((1, 1))
        } else {
            None
        };
        let ok = present(
            &mut self.pool,
            &mut self.solid,
            surface,
            scale,
            format,
            size,
            |c, _, _| {
                for chunk in c.chunks_exact_mut(4) {
                    chunk.copy_from_slice(&px);
                }
            },
        );
        self.has_content |= ok;
    }
}

struct SlideWin {
    scale: Scale,
    window: Window,
    paint: Paint,
    configured: bool,
    fullscreen: bool,
    output: Option<wl_output::WlOutput>,
}

struct LayerBuf {
    alpha: Option<WpAlphaModifierSurfaceV1>,
    scale: Scale,
    layer: LayerSurface,
    paint: Paint,
    configured: bool,
}

impl Drop for LayerBuf {
    fn drop(&mut self) {
        // Los objetos asociados deben destruirse antes que la wl_surface (que destruye `layer`).
        if let Some(a) = self.alpha.take() {
            a.destroy();
        }
        self.scale.destroy();
    }
}

/// Crea un búfer (del tamaño físico de la superficie o `size`), lo rellena y hace commit.
fn present(
    pool: &mut SlotPool,
    slot: &mut Option<Buffer>,
    surface: &wl_surface::WlSurface,
    scale: &Scale,
    format: wl_shm::Format,
    size: Option<(u32, u32)>,
    fill: impl FnOnce(&mut [u8], u32, u32),
) -> bool {
    let (w, h) = size.unwrap_or_else(|| scale.physical());
    let Ok((buffer, canvas)) = pool.create_buffer(w as i32, h as i32, w as i32 * 4, format) else {
        eprintln!("tela: no se pudo reservar memoria para {w}x{h}");
        return false;
    };
    fill(canvas, w, h);
    scale.apply(surface);
    surface.damage_buffer(0, 0, w as i32, h as i32);
    if buffer.attach_to(surface).is_err() {
        return false;
    }
    surface.commit();
    *slot = Some(buffer);
    true
}

// ---------------------------------------------------------------------------------------------
// Cronómetro
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Stopwatch {
    since: Option<Instant>,
    acc: Duration,
    touched: bool,
}

impl Stopwatch {
    fn elapsed(&self) -> Duration {
        self.acc + self.since.map_or(Duration::ZERO, |s| s.elapsed())
    }
    fn running(&self) -> bool {
        self.since.is_some()
    }
    fn toggle(&mut self) {
        self.touched = true;
        match self.since.take() {
            Some(s) => self.acc += s.elapsed(),
            None => self.since = Some(Instant::now()),
        }
    }
    fn reset(&mut self) {
        self.touched = true;
        self.acc = Duration::ZERO;
        self.since = None;
    }
    /// Arranca solo la primera vez que se avanza de diapositiva (si no se tocó antes).
    fn auto_start(&mut self) {
        if !self.touched {
            self.touched = true;
            self.since = Some(Instant::now());
        }
    }
}

fn format_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

// ---------------------------------------------------------------------------------------------
// Estado de la aplicación
// ---------------------------------------------------------------------------------------------

pub struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    viewporter: Option<WpViewporter>,
    fractional_mgr: Option<WpFractionalScaleManagerV1>,
    alpha_mgr: Option<WpAlphaModifierV1>,
    qh: QueueHandle<App>,

    slide: SlideWin,
    notes: Option<LayerBuf>,
    hud: Option<LayerBuf>,
    overlay_output: Option<wl_output::WlOutput>,

    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    ctrl: bool,
    scroll_acc: f64,

    renderer: Renderer,
    info: Info,

    page: u32,
    offset: i32,
    opacity: f32,
    ink: Ink,
    notes_visible: bool,
    hud_visible: bool,
    blank: bool,
    monitor: Option<String>,

    timer: Stopwatch,
    goto: String,
    flash: Option<(String, Instant)>,
    hud_last: String,

    exit: bool,
    debug: bool,
}

pub fn run(args: &Args, slides: PathBuf, notes: Option<PathBuf>) -> Result<(), Box<dyn Error>> {
    let title = args
        .slides
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let (tx, rx): (_, Channel<Image>) = channel::channel();
    let (renderer, info) = render::spawn(slides, notes, tx)?;
    if args.notes.is_some() && info.notes_pages == 0 {
        eprintln!("tela: el archivo de notas no tiene páginas");
    }

    let conn = Connection::connect_to_env().map_err(|_| {
        "no se encontró una sesión Wayland (WAYLAND_DISPLAY). tela necesita Hyprland u otro compositor Wayland."
    })?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let xdg = XdgShell::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(
        |_| "el compositor no soporta wlr-layer-shell (necesario para las notas superpuestas)",
    )?;
    let shm = Shm::bind(&globals, &qh)?;
    let viewporter = globals
        .bind::<WpViewporter, App, NoEvents>(&qh, 1..=1, NoEvents)
        .ok();
    let fractional_mgr = viewporter.as_ref().and_then(|_| {
        globals
            .bind::<WpFractionalScaleManagerV1, App, NoEvents>(&qh, 1..=1, NoEvents)
            .ok()
    });
    let alpha_mgr = globals
        .bind::<WpAlphaModifierV1, App, NoEvents>(&qh, 1..=1, NoEvents)
        .ok();

    let surface = compositor.create_surface(&qh);
    let scale = Scale::new(
        &surface,
        Which::Slide,
        viewporter.as_ref(),
        fractional_mgr.as_ref(),
        &qh,
    );
    let window = xdg.create_window(surface, WindowDecorations::RequestServer, &qh);
    // Este título es el que aparece en el selector de ventanas de Meet/Zoom.
    window.set_title(format!("tela — {title}"));
    window.set_app_id("tela");
    window.set_min_size(Some((320, 180)));
    let slide_paint = Paint::new(&shm).ok_or("no se pudo crear la memoria compartida")?;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm,
        viewporter,
        fractional_mgr,
        alpha_mgr,
        qh: qh.clone(),
        slide: SlideWin {
            scale,
            window,
            paint: slide_paint,
            configured: false,
            fullscreen: false,
            output: None,
        },
        notes: None,
        hud: None,
        overlay_output: None,
        keyboard: None,
        pointer: None,
        ctrl: false,
        scroll_acc: 0.0,
        renderer,
        info,
        page: (args.start_page - 1).min(info.slide_pages - 1),
        offset: args.offset,
        opacity: args.opacity,
        ink: args.ink,
        notes_visible: info.notes_pages > 0,
        hud_visible: true,
        blank: false,
        monitor: args.monitor.clone(),
        timer: Stopwatch::default(),
        goto: String::new(),
        flash: None,
        hud_last: String::new(),
        exit: false,
        debug: std::env::var_os("TELA_DEBUG").is_some(),
    };

    // Recibir la información de monitores y teclado antes de mostrar la ventana.
    queue.roundtrip(&mut app)?;
    queue.roundtrip(&mut app)?;

    let named = app.named_output();
    if let Some(name) = &app.monitor
        && named.is_none()
    {
        let available: Vec<String> = app
            .output_state
            .outputs()
            .filter_map(|o| app.output_state.info(&o)?.name)
            .collect();
        eprintln!(
            "tela: no existe el monitor «{name}». Disponibles: {}",
            available.join(", ")
        );
    }
    if !args.windowed {
        app.slide.window.set_fullscreen(named.as_ref());
    }
    app.slide.window.commit();
    app.sync_overlay();

    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    WaylandSource::new(conn, queue)
        .insert(handle.clone())
        .map_err(|e| e.error.to_string())?;
    handle
        .insert_source(rx, |event, _, app| {
            if let channel::Event::Msg(image) = event {
                app.on_rendered(image);
            }
        })
        .map_err(|e| e.error.to_string())?;
    handle
        .insert_source(
            Timer::from_duration(Duration::from_millis(500)),
            |_, _, app| {
                app.update_hud(false);
                TimeoutAction::ToDuration(Duration::from_millis(500))
            },
        )
        .map_err(|e| e.error.to_string())?;

    // Respaldo por si el compositor nunca envía la escala preferida.
    handle
        .insert_source(
            Timer::from_duration(Duration::from_millis(400)),
            |_, _, app| {
                app.assume_scales();
                TimeoutAction::Drop
            },
        )
        .map_err(|e| e.error.to_string())?;

    while !app.exit {
        event_loop.dispatch(None, &mut app)?;
    }
    Ok(())
}

impl App {
    // ----- monitores y superposición ---------------------------------------------------------

    fn named_output(&self) -> Option<wl_output::WlOutput> {
        let name = self.monitor.as_deref()?;
        self.output_state
            .outputs()
            .find(|o| self.output_state.info(o).and_then(|i| i.name).as_deref() == Some(name))
    }

    /// Crea/destruye las capas de notas y panel según la visibilidad y el monitor de la
    /// presentación.
    fn sync_overlay(&mut self) {
        let target = self.named_output().or_else(|| self.slide.output.clone());
        if self.overlay_output != target {
            self.notes = None;
            self.hud = None;
            self.overlay_output = target.clone();
        }
        let Some(output) = target else { return };

        let want_notes = self.notes_visible && self.info.notes_pages > 0;
        if want_notes && self.notes.is_none() {
            self.notes = self.make_layer(Which::Notes, &output);
        } else if !want_notes {
            self.notes = None;
        }
        if self.hud_visible && self.hud.is_none() {
            self.hud = self.make_layer(Which::Hud, &output);
        } else if !self.hud_visible {
            self.hud = None;
        }
    }

    fn make_layer(&self, which: Which, output: &wl_output::WlOutput) -> Option<LayerBuf> {
        let qh = &self.qh;
        let surface = self.compositor.create_surface(qh);
        let mut scale = Scale::new(
            &surface,
            which,
            self.viewporter.as_ref(),
            self.fractional_mgr.as_ref(),
            qh,
        );
        // Misma pantalla que la presentación → misma escala, sin esperar al compositor.
        if self.slide.scale.known {
            scale.s120 = self.slide.scale.s120;
            scale.known = true;
        }
        let alpha = match which {
            Which::Notes => self
                .alpha_mgr
                .as_ref()
                .map(|m| m.get_surface(&surface, qh, NoEvents)),
            _ => None,
        };
        let namespace = if which == Which::Notes {
            "tela-notas"
        } else {
            "tela-panel"
        };
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some(namespace),
            Some(output),
        );
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        if which == Which::Notes {
            layer.set_anchor(Anchor::all());
            layer.set_exclusive_zone(-1);
            layer.set_size(0, 0);
        } else {
            layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
            layer.set_margin(HUD_MARGIN, HUD_MARGIN, 0, 0);
            layer.set_size(HUD_W, HUD_H);
        }
        // Región de entrada vacía: los clics atraviesan la capa hacia la presentación.
        if let Ok(region) = Region::new(&self.compositor) {
            layer
                .wl_surface()
                .set_input_region(Some(region.wl_region()));
        }
        layer.commit();
        let paint = Paint::new(&self.shm)?;
        Some(LayerBuf {
            alpha,
            scale,
            layer,
            paint,
            configured: false,
        })
    }

    fn which(&self, surface: &wl_surface::WlSurface) -> Option<Which> {
        if self.slide.window.wl_surface() == surface {
            Some(Which::Slide)
        } else if self
            .notes
            .as_ref()
            .is_some_and(|l| l.layer.wl_surface() == surface)
        {
            Some(Which::Notes)
        } else if self
            .hud
            .as_ref()
            .is_some_and(|l| l.layer.wl_surface() == surface)
        {
            Some(Which::Hud)
        } else {
            None
        }
    }

    fn set_scale(&mut self, which: Which, s120: u32) {
        let scale = match which {
            Which::Slide => Some(&mut self.slide.scale),
            Which::Notes => self.notes.as_mut().map(|l| &mut l.scale),
            Which::Hud => self.hud.as_mut().map(|l| &mut l.scale),
        };
        let Some(scale) = scale else { return };
        let was_known = std::mem::replace(&mut scale.known, true);
        if (scale.s120 == s120 && was_known) || s120 == 0 {
            return;
        }
        scale.s120 = s120;
        // Las páginas rasterizadas a la escala anterior ya no sirven.
        match which {
            Which::Slide => self.slide.paint.cache.clear(),
            Which::Notes => self.notes.iter_mut().for_each(|n| n.paint.cache.clear()),
            Which::Hud => {}
        }
        match which {
            Which::Slide => self.draw_slide(),
            Which::Notes => self.draw_notes(),
            Which::Hud => self.update_hud(true),
        }
        self.schedule();
    }

    fn assume_scales(&mut self) {
        let mut changed = !std::mem::replace(&mut self.slide.scale.known, true);
        if let Some(n) = self.notes.as_mut() {
            changed |= !std::mem::replace(&mut n.scale.known, true);
        }
        if changed {
            self.refresh();
        }
    }

    // ----- páginas y caché -------------------------------------------------------------------

    fn notes_index(&self, slide_page: u32) -> Option<u32> {
        let p = slide_page as i64 + self.offset as i64;
        (0..self.info.notes_pages as i64)
            .contains(&p)
            .then_some(p as u32)
    }

    fn slide_key(&self, page: u32) -> Option<Key> {
        if !self.slide.configured || !self.slide.scale.known || page >= self.info.slide_pages {
            return None;
        }
        let (width, height) = self.slide.scale.physical();
        Some(Key {
            kind: Kind::Slide,
            page,
            width,
            height,
            ink: Ink::Off,
            opacity: 255,
        })
    }

    fn notes_key(&self, slide_page: u32) -> Option<Key> {
        let layer = self
            .notes
            .as_ref()
            .filter(|l| l.configured && l.scale.known)?;
        let page = self.notes_index(slide_page)?;
        let (width, height) = layer.scale.physical();
        // Con wp_alpha_modifier la opacidad la aplica el compositor; si no, se hornea al render.
        let opacity = if layer.alpha.is_some() {
            255
        } else {
            (self.opacity * 255.0).round() as u8
        };
        Some(Key {
            kind: Kind::Notes,
            page,
            width,
            height,
            ink: self.ink,
            opacity,
        })
    }

    /// Pide al hilo de render lo que falte: la página actual y la siguiente.
    fn schedule(&self) {
        let mut keys = Vec::with_capacity(4);
        for p in [Some(self.page), self.page.checked_add(1)]
            .into_iter()
            .flatten()
        {
            keys.extend(self.slide_key(p).filter(|k| !self.slide.paint.has(*k)));
            if let Some(n) = &self.notes {
                keys.extend(self.notes_key(p).filter(|k| !n.paint.has(*k)));
            }
        }
        self.renderer.request(keys);
    }

    fn on_rendered(&mut self, image: Image) {
        let key = image.key;
        self.renderer.delivered(key);
        // Se descartan renders de un tamaño que ya no corresponde (p. ej. tras un cambio de escala).
        match key.kind {
            Kind::Slide if self.slide.scale.physical() == (key.width, key.height) => {
                self.slide.paint.store(&image, wl_shm::Format::Xrgb8888);
            }
            Kind::Notes => match &mut self.notes {
                Some(n) if n.scale.physical() == (key.width, key.height) => {
                    n.paint.store(&image, wl_shm::Format::Argb8888);
                }
                _ => return,
            },
            _ => return,
        }
        drop(image);
        if Some(key) == self.slide_key(self.page) {
            self.draw_slide();
        }
        if Some(key) == self.notes_key(self.page) {
            self.draw_notes();
        }
    }

    // ----- dibujo ----------------------------------------------------------------------------

    fn draw_slide(&mut self) {
        if !self.slide.configured {
            return;
        }
        let key = self.slide_key(self.page);
        let s = &mut self.slide;
        let surface = s.window.wl_surface();
        if self.blank {
            s.paint
                .show_solid(surface, &s.scale, wl_shm::Format::Xrgb8888, [0, 0, 0, 255]);
            return;
        }
        let shown = key.is_some_and(|k| s.paint.show(k, surface, &s.scale));
        if !shown && !s.paint.has_content {
            // Primer cuadro en negro para que la ventana aparezca mientras se renderiza.
            s.paint
                .show_solid(surface, &s.scale, wl_shm::Format::Xrgb8888, [0, 0, 0, 255]);
        }
    }

    fn draw_notes(&mut self) {
        let key = self.notes_key(self.page);
        let in_range = self.notes_index(self.page).is_some();
        let multiplier = (self.opacity as f64 * u32::MAX as f64) as u32;
        let Some(layer) = self.notes.as_mut().filter(|l| l.configured) else {
            return;
        };
        if let Some(alpha) = &layer.alpha {
            alpha.set_multiplier(multiplier);
        }
        let surface = layer.layer.wl_surface();
        let shown = key.is_some_and(|k| layer.paint.show(k, surface, &layer.scale));
        // Sin nota para esta diapositiva → transparente. Si la nota aún se está renderizando,
        // se mantiene la anterior (salvo que no haya nada todavía).
        if !shown && (!in_range || !layer.paint.has_content) {
            layer
                .paint
                .show_solid(surface, &layer.scale, wl_shm::Format::Argb8888, [0; 4]);
        }
    }

    fn apply_opacity(&mut self) {
        let Some(layer) = self.notes.as_mut() else {
            return;
        };
        match &layer.alpha {
            Some(alpha) => {
                alpha.set_multiplier((self.opacity as f64 * u32::MAX as f64) as u32);
                layer.layer.wl_surface().commit();
            }
            None => {
                // La opacidad está horneada: hay que volver a renderizar.
                layer.paint.cache.clear();
                self.draw_notes();
                self.schedule();
            }
        }
    }

    fn hud_text(&self) -> String {
        let mut s = String::new();
        if self.blank {
            s.push_str("● NEGRO   ");
        }
        s.push_str(&format_elapsed(self.timer.elapsed()));
        if !self.timer.running() && self.timer.touched {
            s.push_str(" ‖");
        }
        s.push_str(&format!("   {}/{}", self.page + 1, self.info.slide_pages));
        match &self.flash {
            Some((msg, _)) => s.push_str(&format!("   {msg}")),
            None => {
                if self.offset != 0 {
                    s.push_str(&format!("   notas {:+}", self.offset));
                }
                if let Some(clock) = glib::DateTime::now_local()
                    .ok()
                    .and_then(|d| d.format("%H:%M").ok())
                {
                    s.push_str(&format!("   {clock}"));
                }
            }
        }
        s
    }

    fn update_hud(&mut self, force: bool) {
        if let Some((_, t)) = &self.flash
            && t.elapsed() > FLASH_FOR
            && self.goto.is_empty()
        {
            self.flash = None;
        }
        if !self.hud.as_ref().is_some_and(|h| h.configured) {
            return;
        }
        let text = self.hud_text();
        if !force && text == self.hud_last {
            return;
        }
        let blank = self.blank;
        let hud = self.hud.as_mut().unwrap();
        let s = hud.scale.s120 as f64 / 120.0;
        let (lw, lh) = (hud.scale.logical.0 as f64, hud.scale.logical.1 as f64);
        let paint = &mut hud.paint;
        let ok = present(
            &mut paint.pool,
            &mut paint.solid,
            hud.layer.wl_surface(),
            &hud.scale,
            wl_shm::Format::Argb8888,
            None,
            |canvas, w, h| draw_hud_text(canvas, w, h, s, lw, lh, &text, blank),
        );
        paint.has_content |= ok;
        self.hud_last = text;
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
        self.update_hud(false);
    }

    /// Redibuja todo lo que depende de la página actual y precarga las vecinas.
    fn refresh(&mut self) {
        self.draw_slide();
        self.draw_notes();
        self.schedule();
        self.update_hud(false);
    }

    // ----- acciones --------------------------------------------------------------------------

    fn go_to(&mut self, page: i64) {
        let page = page.clamp(0, self.info.slide_pages as i64 - 1) as u32;
        if page != self.page {
            self.page = page;
            self.timer.auto_start();
            self.refresh();
        }
    }

    fn step(&mut self, delta: i64) {
        self.go_to(self.page as i64 + delta);
    }

    fn set_opacity(&mut self, value: f32) {
        self.opacity = (value * 20.0).round().clamp(1.0, 20.0) / 20.0;
        self.flash(format!("opacidad {}%", (self.opacity * 100.0).round()));
        self.apply_opacity();
    }

    fn set_offset(&mut self, offset: i32) {
        self.offset = offset;
        self.flash(format!("desfase notas {offset:+}"));
        self.draw_notes();
        self.schedule();
    }

    fn toggle_notes(&mut self) {
        if self.info.notes_pages == 0 {
            self.flash("sin archivo de notas");
            return;
        }
        self.notes_visible = !self.notes_visible;
        self.sync_overlay();
        self.flash(if self.notes_visible {
            "notas visibles"
        } else {
            "notas ocultas"
        });
    }

    fn toggle_fullscreen(&mut self, on: bool) {
        if on {
            let output = self.named_output().or_else(|| self.slide.output.clone());
            self.slide.window.set_fullscreen(output.as_ref());
        } else {
            self.slide.window.unset_fullscreen();
        }
    }

    fn on_key(&mut self, event: KeyEvent) {
        let k = event.keysym;
        if self.debug {
            eprintln!("tela: tecla {:?} {:?}", k.name(), event.utf8);
        }

        // Número de diapositiva + Intro
        if !self.ctrl
            && let Some(text) = &event.utf8
            && text.len() == 1
            && text.as_bytes()[0].is_ascii_digit()
        {
            if self.goto.len() < 5 {
                self.goto.push_str(text);
            }
            let msg = format!("ir a: {}_", self.goto);
            self.flash(msg);
            return;
        }
        if !self.goto.is_empty() {
            match k {
                Keysym::Return | Keysym::KP_Enter => {
                    let n: i64 = self.goto.parse().unwrap_or(1);
                    self.goto.clear();
                    self.flash = None;
                    self.go_to(n - 1);
                    self.update_hud(false);
                    return;
                }
                Keysym::Escape | Keysym::BackSpace => {
                    self.goto.clear();
                    self.flash("cancelado");
                    return;
                }
                _ => {
                    self.goto.clear();
                    self.flash = None;
                }
            }
        }

        match k {
            Keysym::Right if self.ctrl => self.set_offset(self.offset + 1),
            Keysym::Left if self.ctrl => self.set_offset(self.offset - 1),
            Keysym::Right
            | Keysym::Down
            | Keysym::Page_Down
            | Keysym::KP_Next
            | Keysym::KP_Right
            | Keysym::KP_Down
            | Keysym::space
            | Keysym::Return
            | Keysym::KP_Enter
            | Keysym::n
            | Keysym::N => self.step(1),
            Keysym::Left
            | Keysym::Up
            | Keysym::Page_Up
            | Keysym::KP_Prior
            | Keysym::KP_Left
            | Keysym::KP_Up
            | Keysym::BackSpace
            | Keysym::p
            | Keysym::P => self.step(-1),
            Keysym::Home | Keysym::KP_Home => self.go_to(0),
            Keysym::End | Keysym::KP_End => self.go_to(i64::MAX),

            Keysym::h | Keysym::H => self.toggle_notes(),
            Keysym::plus | Keysym::equal | Keysym::KP_Add => self.set_opacity(self.opacity + 0.05),
            Keysym::minus | Keysym::underscore | Keysym::KP_Subtract => {
                self.set_opacity(self.opacity - 0.05)
            }
            Keysym::i | Keysym::I => {
                self.ink = self.ink.next();
                if let Some(n) = &mut self.notes {
                    n.paint.cache.clear();
                }
                self.flash(self.ink.label());
                self.draw_notes();
                self.schedule();
            }
            Keysym::bracketleft => self.set_offset(self.offset - 1),
            Keysym::bracketright => self.set_offset(self.offset + 1),

            Keysym::t | Keysym::T => {
                self.timer.toggle();
                self.flash(if self.timer.running() {
                    "cronómetro ▶"
                } else {
                    "cronómetro ‖"
                });
            }
            Keysym::r | Keysym::R => {
                self.timer.reset();
                self.flash("cronómetro a cero");
            }
            Keysym::c | Keysym::C => {
                self.hud_visible = !self.hud_visible;
                self.sync_overlay();
            }

            Keysym::b | Keysym::B | Keysym::period | Keysym::KP_Decimal => {
                self.blank = !self.blank;
                self.draw_slide();
                self.update_hud(false);
            }
            Keysym::f | Keysym::F | Keysym::F11 => self.toggle_fullscreen(!self.slide.fullscreen),
            Keysym::F5 => self.toggle_fullscreen(true),
            Keysym::Escape => self.toggle_fullscreen(false),
            Keysym::q | Keysym::Q => self.exit = true,
            _ => {}
        }
    }
}

/// Dibuja el panel: texto alineado a la derecha sobre una píldora oscura semitransparente.
#[allow(clippy::too_many_arguments)]
fn draw_hud_text(
    canvas: &mut [u8],
    w: u32,
    h: u32,
    scale: f64,
    lw: f64,
    lh: f64,
    text: &str,
    alert: bool,
) {
    canvas.fill(0);
    let Ok(mut surface) = cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32)
    else {
        return;
    };
    {
        let Ok(cr) = cairo::Context::new(&surface) else {
            return;
        };
        cr.scale(scale, scale);
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(lh * 0.47);
        let (Ok(ext), Ok(fext)) = (cr.text_extents(text), cr.font_extents()) else {
            return;
        };
        let pad = lh * 0.4;
        let box_w = (ext.x_advance() + pad * 2.0).min(lw);
        let x0 = lw - box_w;
        let r = lh / 2.0;

        cr.new_sub_path();
        cr.arc(
            x0 + r,
            r,
            r,
            std::f64::consts::FRAC_PI_2,
            3.0 * std::f64::consts::FRAC_PI_2,
        );
        cr.arc(
            lw - r,
            r,
            r,
            -std::f64::consts::FRAC_PI_2,
            std::f64::consts::FRAC_PI_2,
        );
        cr.close_path();
        if alert {
            cr.set_source_rgba(0.55, 0.05, 0.05, 0.85);
        } else {
            cr.set_source_rgba(0.06, 0.06, 0.08, 0.78);
        }
        let _ = cr.fill();

        cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
        let baseline = (lh - (fext.ascent() + fext.descent())) / 2.0 + fext.ascent();
        cr.move_to(x0 + pad, baseline);
        let _ = cr.show_text(text);
    }
    surface.flush();
    let stride = surface.stride() as usize;
    if let Ok(data) = surface.data() {
        let row = w as usize * 4;
        for (y, dst) in canvas.chunks_exact_mut(row).enumerate() {
            dst.copy_from_slice(&data[y * stride..y * stride + row]);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Manejadores Wayland
// ---------------------------------------------------------------------------------------------

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        // Solo se usa si no hay escala fraccional.
        if self.fractional_mgr.is_none()
            && let Some(which) = self.which(surface)
        {
            self.set_scale(which, factor.max(1) as u32 * 120);
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        if self.slide.window.wl_surface() == surface {
            self.slide.output = Some(output.clone());
            self.sync_overlay();
        }
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        if self.slide.configured {
            self.sync_overlay();
        }
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.slide.output.as_ref() == Some(&output) {
            self.slide.output = None;
        }
        if self.overlay_output.as_ref() == Some(&output) {
            self.notes = None;
            self.hud = None;
            self.overlay_output = None;
        }
        self.sync_overlay();
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        self.slide.fullscreen = configure.is_fullscreen();
        let size = match configure.new_size {
            (Some(w), Some(h)) => (w.get(), h.get()),
            _ if self.slide.configured => self.slide.scale.logical,
            _ => (1280, 720),
        };
        if self.slide.scale.logical != size {
            self.slide.paint.cache.clear();
        }
        self.slide.scale.logical = size;
        self.slide.configured = true;
        self.refresh();
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        match self.which(layer.wl_surface()) {
            Some(Which::Notes) => self.notes = None,
            Some(Which::Hud) => self.hud = None,
            _ => {}
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let which = self.which(layer.wl_surface());
        let target = match which {
            Some(Which::Notes) => self.notes.as_mut(),
            Some(Which::Hud) => self.hud.as_mut(),
            _ => None,
        };
        let Some(target) = target else { return };
        let (w, h) = configure.new_size;
        let size = match (w, h) {
            (0, _) | (_, 0) if which == Some(Which::Hud) => (HUD_W, HUD_H),
            (0, _) | (_, 0) => self.slide.scale.logical,
            _ => (w, h),
        };
        if target.scale.logical != size {
            target.paint.cache.clear();
        }
        target.scale.logical = size;
        target.configured = true;
        match which {
            Some(Which::Notes) => {
                self.draw_notes();
                self.schedule();
            }
            _ => self.update_hud(true),
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(k) = self.keyboard.take()
        {
            k.release();
        }
        if capability == Capability::Pointer
            && let Some(p) = self.pointer.take()
        {
            p.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.ctrl = false;
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.on_key(event);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.ctrl = modifiers.ctrl;
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if &event.surface != self.slide.window.wl_surface() {
                continue;
            }
            match event.kind {
                // En pantalla completa se oculta el cursor para que no aparezca en lo compartido.
                PointerEventKind::Enter { serial } if self.slide.fullscreen => {
                    pointer.set_cursor(serial, None, 0, 0);
                }
                PointerEventKind::Press {
                    button: BTN_LEFT, ..
                } => self.step(1),
                PointerEventKind::Press {
                    button: BTN_RIGHT, ..
                } => self.step(-1),
                PointerEventKind::Axis { vertical, .. } => {
                    self.scroll_acc += if vertical.value120 != 0 {
                        vertical.value120 as f64 / 120.0
                    } else if vertical.discrete != 0 {
                        vertical.discrete as f64
                    } else {
                        vertical.absolute / 40.0
                    };
                    while self.scroll_acc >= 1.0 {
                        self.scroll_acc -= 1.0;
                        self.step(1);
                    }
                    while self.scroll_acc <= -1.0 {
                        self.scroll_acc += 1.0;
                        self.step(-1);
                    }
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(App);

/// Objetos sin eventos (viewporter, alpha-modifier, gestor de escala fraccional).
struct NoEvents;

struct FracData(Which);

impl Dispatch2<WpFractionalScaleV1, App> for FracData {
    fn event(
        &self,
        app: &mut App,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            app.set_scale(self.0, scale);
        }
    }
}

macro_rules! no_events {
    ($($ty:ty),*) => {$(
        impl Dispatch2<$ty, App> for NoEvents {
            fn event(&self, _: &mut App, _: &$ty, _: <$ty as smithay_client_toolkit::reexports::client::Proxy>::Event, _: &Connection, _: &QueueHandle<App>) {}
        }
    )*};
}
no_events!(
    WpViewporter,
    WpViewport,
    WpFractionalScaleManagerV1,
    WpAlphaModifierV1,
    WpAlphaModifierSurfaceV1
);
