//! Línea de comandos (sin dependencias externas para mantener el binario pequeño).

use std::path::{Path, PathBuf};

use crate::render::Ink;

pub const HELP: &str = "\
tela — notas privadas superpuestas para presentar compartiendo pantalla (Hyprland/Wayland)

USO:
    tela                                         Abrir el lanzador (interfaz gráfica)
    tela <presentación> [--notas <archivo>] [opciones]

  <presentación> y <archivo de notas> pueden ser .pdf o .ppt/.pptx/.odp
  (los PowerPoint se convierten una sola vez con LibreOffice y quedan en caché).

  Si no indicas --notas, se busca automáticamente junto a la presentación:
  <nombre>.notas.pdf, <nombre>-notas.pdf o <nombre>_notas.pdf

OPCIONES:
    -n, --notas <archivo>     PDF/PPT de notas (página N = notas de la diapositiva N)
    -d, --desfase <N>         Desfase de las notas respecto a las diapositivas (p. ej. 1 o -1)
    -o, --opacidad <valor>    Opacidad inicial de las notas: 0.05–1 o 5–100 (por defecto 0.6)
        --tinta-clara         Texto de las notas en claro (para diapositivas oscuras)
        --sin-tinta           Mostrar la página de notas tal cual (con fondo blanco)
    -p, --pagina <N>          Empezar en la diapositiva N (por defecto 1)
    -m, --monitor <nombre>    Monitor donde presentar (p. ej. DP-1, HDMI-A-1; ver `hyprctl monitors`)
    -w, --ventana             Empezar en ventana en lugar de pantalla completa
    -h, --help                Mostrar esta ayuda

TECLAS (con la ventana de la presentación enfocada):
    → ↓ AvPág Espacio Intro clic-izq rueda   Siguiente diapositiva
    ← ↑ RePág Retroceso clic-der rueda       Diapositiva anterior
    Inicio / Fin                             Primera / última
    número + Intro                           Ir a la diapositiva (Esc cancela)
    H                                        Mostrar/ocultar notas
    + / -                                    Subir/bajar opacidad de las notas
    I                                        Tinta oscura → tinta clara → página completa
    [ / ]                                    Desfase de notas -1 / +1
    T / R                                    Cronómetro: iniciar-pausar / reiniciar
    C                                        Mostrar/ocultar el panel de cronómetro
    B o .                                    Pantalla negra (lo que ve la audiencia)
    F / F11 / F5 / Esc                       Pantalla completa sí/no (F5 activa, Esc sale)
    Q / Ctrl+Q                               Salir

IMPORTANTE: en Meet/Zoom comparte la VENTANA «tela — …», no la pantalla completa.
";

#[derive(Debug, Clone)]
pub struct Args {
    pub slides: PathBuf,
    pub notes: Option<PathBuf>,
    pub offset: i32,
    pub opacity: f32,
    pub ink: Ink,
    pub start_page: u32,
    pub monitor: Option<String>,
    pub windowed: bool,
}

pub enum Parsed {
    Run(Args),
    Help,
}

pub fn parse() -> Result<Parsed, String> {
    parse_from(std::env::args().skip(1))
}

pub(crate) fn parse_from(mut it: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let mut slides = None;
    let mut notes = None;
    let mut offset = 0i32;
    let mut opacity = 0.6f32;
    let mut ink = Ink::Dark;
    let mut start_page = 1u32;
    let mut monitor = None;
    let mut windowed = false;

    while let Some(arg) = it.next() {
        // Admite tanto `--opcion valor` como `--opcion=valor`.
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or_else(|| format!("falta el valor de {name}"))
        };
        match flag.as_str() {
            "-h" | "--help" | "--ayuda" => return Ok(Parsed::Help),
            "-n" | "--notas" | "--notes" => notes = Some(PathBuf::from(value(&flag)?)),
            "-d" | "--desfase" | "--offset" => {
                let v = value(&flag)?;
                offset = v.parse().map_err(|_| format!("desfase inválido: {v}"))?;
            }
            "-o" | "--opacidad" | "--opacity" => {
                let v = value(&flag)?;
                opacity = parse_opacity(&v)?;
            }
            "--sin-tinta" | "--no-ink" => ink = Ink::Off,
            "--tinta-clara" | "--light-ink" => ink = Ink::Light,
            "-p" | "--pagina" | "--página" | "--page" => {
                let v = value(&flag)?;
                start_page = v
                    .parse()
                    .ok()
                    .filter(|p| *p >= 1)
                    .ok_or_else(|| format!("página inválida: {v}"))?;
            }
            "-m" | "--monitor" => monitor = Some(value(&flag)?),
            "-w" | "--ventana" | "--windowed" => windowed = true,
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(format!("opción desconocida: {s}\nUsa `tela --help`."));
            }
            _ => {
                if slides.is_some() {
                    return Err(format!(
                        "sobra el argumento «{arg}». ¿Querías usar --notas {arg}?"
                    ));
                }
                slides = Some(PathBuf::from(arg));
            }
        }
    }

    let slides =
        slides.ok_or("falta la presentación.\nUso: tela <presentación> [--notas <archivo>]")?;
    let notes = notes.or_else(|| find_notes(&slides));

    Ok(Parsed::Run(Args {
        slides,
        notes,
        offset,
        opacity,
        ink,
        start_page,
        monitor,
        windowed,
    }))
}

fn parse_opacity(v: &str) -> Result<f32, String> {
    let n: f32 = v
        .trim_end_matches('%')
        .replace(',', ".")
        .parse()
        .map_err(|_| format!("opacidad inválida: {v}"))?;
    let n = if n > 1.0 { n / 100.0 } else { n };
    if !(0.0..=1.0).contains(&n) {
        return Err(format!("opacidad fuera de rango: {v}"));
    }
    Ok(n.max(0.05))
}

/// Busca `<nombre>.notas.pdf`, `<nombre>-notas.pdf` o `<nombre>_notas.pdf` junto a la presentación.
pub fn find_notes(slides: &Path) -> Option<PathBuf> {
    let stem = slides.file_stem()?.to_str()?;
    let dir = slides.parent().unwrap_or(Path::new(""));
    ["pdf", "pptx", "ppt", "odp"].iter().find_map(|ext| {
        [".notas", "-notas", "_notas"].iter().find_map(|sep| {
            let p = dir.join(format!("{stem}{sep}.{ext}"));
            p.is_file().then_some(p)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Args {
        match parse_from(args.iter().map(|s| s.to_string())).unwrap() {
            Parsed::Run(a) => a,
            Parsed::Help => panic!("help"),
        }
    }

    #[test]
    fn parses_flags() {
        let a = run(&[
            "deck.pdf",
            "--notas=n.pdf",
            "-d",
            "-1",
            "-o",
            "40",
            "--sin-tinta",
            "-w",
        ]);
        assert_eq!(a.slides, PathBuf::from("deck.pdf"));
        assert_eq!(a.notes, Some(PathBuf::from("n.pdf")));
        assert_eq!(a.offset, -1);
        assert!((a.opacity - 0.4).abs() < 1e-6);
        assert_eq!(a.ink, Ink::Off);
        assert!(a.windowed);
    }

    #[test]
    fn opacity_forms() {
        assert!((parse_opacity("0,5").unwrap() - 0.5).abs() < 1e-6);
        assert!((parse_opacity("75%").unwrap() - 0.75).abs() < 1e-6);
        assert!(parse_opacity("150").is_err());
    }

    #[test]
    fn rejects_unknown() {
        assert!(parse_from(["x.pdf", "--foo"].iter().map(|s| s.to_string())).is_err());
    }
}
