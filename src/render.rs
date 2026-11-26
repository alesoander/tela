//! Hilo de render: abre los PDF con poppler y rasteriza páginas al tamaño exacto de la pantalla.
//!
//! El hilo principal (Wayland) nunca se bloquea renderizando: pide páginas por orden de prioridad
//! (actual, siguiente, anterior) y el hilo devuelve imágenes listas para copiar al búfer `wl_shm`.
//! La cola es "la última petición manda": al cambiar de diapositiva se descartan las precargas
//! obsoletas.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use cairo::{Context, Format, ImageSurface, ImageSurfaceDataOwned};
use poppler::Document;
use smithay_client_toolkit::reexports::calloop::channel::Sender;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Slide,
    Notes,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    pub kind: Kind,
    pub page: u32,
    pub width: u32,
    pub height: u32,
    pub ink: Ink,
    /// Opacidad horneada (255 = opaco); solo si el compositor no soporta wp_alpha_modifier.
    pub opacity: u8,
}

/// Cómo se muestran las notas sobre la diapositiva.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Ink {
    /// El papel blanco se vuelve transparente; el texto conserva sus colores.
    Dark,
    /// Igual, pero con los colores invertidos (texto claro para diapositivas oscuras).
    Light,
    /// La página tal cual, con su fondo.
    Off,
}

impl Ink {
    pub fn next(self) -> Ink {
        match self {
            Ink::Dark => Ink::Light,
            Ink::Light => Ink::Off,
            Ink::Off => Ink::Dark,
        }
    }

    /// Nombre estable para guardar en la configuración.
    pub fn id(self) -> &'static str {
        match self {
            Ink::Dark => "oscura",
            Ink::Light => "clara",
            Ink::Off => "pagina",
        }
    }

    pub fn from_id(id: &str) -> Option<Ink> {
        [Ink::Dark, Ink::Light, Ink::Off]
            .into_iter()
            .find(|i| i.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            Ink::Dark => "tinta oscura",
            Ink::Light => "tinta clara",
            Ink::Off => "página completa",
        }
    }
}

/// Imagen ARGB32 premultiplicada (mismo formato en memoria que `wl_shm` Argb8888).
pub struct Image {
    pub key: Key,
    pub stride: usize,
    data: ImageSurfaceDataOwned,
}

impl Image {
    pub fn bytes(&self) -> &[u8] {
        self.data.as_ref()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Info {
    pub slide_pages: u32,
    pub notes_pages: u32,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Key>,
    in_flight: Option<Key>,
    /// Renderizadas y enviadas, pero que el hilo principal aún no recibió.
    sent: Vec<Key>,
    quit: bool,
}

pub struct Renderer {
    shared: Arc<(Mutex<Queue>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl Renderer {
    /// Reemplaza la cola de trabajo por `keys` (en orden de prioridad).
    pub fn request(&self, keys: impl IntoIterator<Item = Key>) {
        let (lock, cvar) = &*self.shared;
        let mut q = lock.lock().unwrap();
        q.jobs.clear();
        for k in keys {
            if q.in_flight != Some(k) && !q.sent.contains(&k) && !q.jobs.contains(&k) {
                q.jobs.push_back(k);
            }
        }
        if !q.jobs.is_empty() {
            cvar.notify_one();
        }
    }
}

impl Renderer {
    /// El hilo principal confirma que recibió la imagen `key`.
    pub fn delivered(&self, key: Key) {
        self.shared.0.lock().unwrap().sent.retain(|k| *k != key);
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.shared;
        lock.lock().unwrap().quit = true;
        cvar.notify_one();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn spawn(
    slides: PathBuf,
    notes: Option<PathBuf>,
    results: Sender<Image>,
) -> Result<(Renderer, Info), String> {
    let shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
    let (info_tx, info_rx) = std::sync::mpsc::channel();

    let worker_shared = shared.clone();
    let thread = std::thread::Builder::new()
        .name("tela-render".into())
        .spawn(move || {
            // Los objetos de poppler/glib no son `Send`: se crean y viven solo en este hilo.
            let slides_doc = match open(&slides) {
                Ok(d) => d,
                Err(e) => return drop(info_tx.send(Err(e))),
            };
            let notes_doc = match notes.as_ref().map(open).transpose() {
                Ok(d) => d,
                Err(e) => return drop(info_tx.send(Err(e))),
            };
            let info = Info {
                slide_pages: slides_doc.n_pages().max(0) as u32,
                notes_pages: notes_doc.as_ref().map_or(0, |d| d.n_pages().max(0) as u32),
            };
            if info.slide_pages == 0 {
                return drop(info_tx.send(Err(format!("«{}» no tiene páginas", slides.display()))));
            }
            let _ = info_tx.send(Ok(info));
            worker(&worker_shared, &slides_doc, notes_doc.as_ref(), &results);
        })
        .map_err(|e| format!("no se pudo crear el hilo de render: {e}"))?;

    let info = info_rx
        .recv()
        .map_err(|_| "el hilo de render terminó inesperadamente".to_string())??;
    Ok((
        Renderer {
            shared,
            thread: Some(thread),
        },
        info,
    ))
}

fn open(path: &PathBuf) -> Result<Document, String> {
    let uri = glib::filename_to_uri(path, None).map_err(|e| e.to_string())?;
    Document::from_file(&uri, None)
        .map_err(|e| format!("no se pudo abrir «{}»: {e}", path.display()))
}

fn worker(
    shared: &(Mutex<Queue>, Condvar),
    slides: &Document,
    notes: Option<&Document>,
    results: &Sender<Image>,
) {
    let (lock, cvar) = shared;
    let debug = std::env::var_os("TELA_DEBUG").is_some();
    loop {
        let key = {
            let mut q = lock.lock().unwrap();
            loop {
                if q.quit {
                    return;
                }
                if let Some(k) = q.jobs.pop_front() {
                    q.in_flight = Some(k);
                    break k;
                }
                q = cvar.wait(q).unwrap();
            }
        };

        let doc = match key.kind {
            Kind::Slide => Some(slides),
            Kind::Notes => notes,
        };
        let t0 = std::time::Instant::now();
        let image = doc.and_then(|d| render_page(d, key));
        if debug {
            eprintln!(
                "tela: render {:?} p{} {}x{} en {:?}",
                key.kind,
                key.page + 1,
                key.width,
                key.height,
                t0.elapsed()
            );
        }
        {
            let mut q = lock.lock().unwrap();
            q.in_flight = None;
            if image.is_some() {
                q.sent.push(key);
            }
        }

        if let Some(image) = image
            && results.send(image).is_err()
        {
            return; // el bucle principal ya terminó
        }
    }
}

fn render_page(doc: &Document, key: Key) -> Option<Image> {
    let page = doc.page(key.page as i32)?;
    let (pw, ph) = page.size();
    if pw <= 0.0 || ph <= 0.0 || key.width == 0 || key.height == 0 {
        return None;
    }
    let (w, h) = (key.width as f64, key.height as f64);

    let mut surface =
        ImageSurface::create(Format::ARgb32, key.width as i32, key.height as i32).ok()?;
    {
        let cr = Context::new(&surface).ok()?;
        // Encaje "contain" centrado y alineado a píxel; notas y diapositivas usan la misma
        // geometría para que coincidan si tienen la misma proporción.
        let s = (w / pw).min(h / ph);
        let dx = ((w - pw * s) / 2.0).round();
        let dy = ((h - ph * s) / 2.0).round();

        if key.kind == Kind::Slide {
            cr.set_source_rgb(0.0, 0.0, 0.0);
            cr.paint().ok()?;
        }
        cr.translate(dx, dy);
        cr.scale(s, s);
        cr.rectangle(0.0, 0.0, pw, ph);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.fill().ok()?;
        page.render(&cr);
    }
    surface.flush();

    if key.ink != Ink::Off {
        let mut data = surface.data().ok()?;
        white_to_alpha(&mut data);
        if key.ink == Ink::Light {
            invert_lightness(&mut data);
        }
    }
    if key.opacity < 255 {
        let mut data = surface.data().ok()?;
        scale_alpha(&mut data, key.opacity);
    }

    let stride = surface.stride() as usize;
    let data = surface.take_data().ok()?;
    Some(Image { key, stride, data })
}

/// "Color a alfa" respecto al blanco (como en GIMP): el papel blanco desaparece y el texto
/// queda nítido con sus colores originales al componerse sobre una superficie blanca.
///
/// Opera sobre ARGB32 premultiplicado en orden de bytes little-endian (B, G, R, A).
pub fn white_to_alpha(data: &mut [u8]) {
    for px in data.chunks_exact_mut(4) {
        // Componer sobre blanco: c' = c + (255 - a)
        let inv = 255 - px[3];
        let b = px[0].saturating_add(inv);
        let g = px[1].saturating_add(inv);
        let r = px[2].saturating_add(inv);
        let m = b.min(g).min(r);
        px[0] = b - m;
        px[1] = g - m;
        px[2] = r - m;
        px[3] = 255 - m;
    }
}

/// Invierte la luminosidad (L → 1 − L en HSL) conservando tono, saturación y alfa: el negro pasa
/// a blanco y los colores se aclaran sin cambiar de tono. En premultiplicado: c' = c + a − max − min.
pub fn invert_lightness(data: &mut [u8]) {
    for px in data.chunks_exact_mut(4) {
        let (b, g, r, a) = (px[0] as i16, px[1] as i16, px[2] as i16, px[3] as i16);
        let shift = a - b.max(g).max(r) - b.min(g).min(r);
        px[0] = (b + shift) as u8;
        px[1] = (g + shift) as u8;
        px[2] = (r + shift) as u8;
    }
}

/// Multiplica una imagen premultiplicada por una opacidad global (0–255).
pub fn scale_alpha(data: &mut [u8], opacity: u8) {
    let k = opacity as u16;
    for b in data.iter_mut() {
        *b = ((*b as u16 * k + 127) / 255) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_becomes_transparent_black_stays() {
        let mut px = [255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 0, 0, 0, 255, 255];
        white_to_alpha(&mut px);
        assert_eq!(&px[0..4], &[0, 0, 0, 0], "blanco → transparente");
        assert_eq!(&px[4..8], &[0, 0, 0, 255], "negro → opaco");
        assert_eq!(&px[8..12], &[0, 0, 0, 0], "transparente sigue transparente");
        assert_eq!(&px[12..16], &[0, 0, 255, 255], "rojo puro se mantiene");
    }

    #[test]
    fn grey_reconstructs_over_white() {
        // Gris 50 % sobre blanco debe reconstruir el mismo gris.
        let mut px = [128, 128, 128, 255];
        white_to_alpha(&mut px);
        let a = px[3] as u16;
        let over_white = px[0] as u16 + (255 - a);
        assert_eq!(over_white, 128);
    }

    #[test]
    fn light_ink_turns_black_text_white() {
        let mut px = [0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 255, 255];
        white_to_alpha(&mut px);
        invert_lightness(&mut px);
        assert_eq!(&px[0..4], &[255, 255, 255, 255], "texto negro → blanco");
        assert_eq!(&px[4..8], &[0, 0, 0, 0], "papel sigue transparente");
        assert_eq!(&px[8..12], &[0, 0, 255, 255], "rojo puro conserva el tono");
    }

    #[test]
    fn opacity_scales_all_channels() {
        let mut px = [255u8, 128, 0, 255];
        scale_alpha(&mut px, 128);
        assert_eq!(px, [128, 64, 0, 128]);
    }
}
