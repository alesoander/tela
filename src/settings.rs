//! Ajustes recordados por presentación, en `~/.config/tela/presentaciones.conf`.
//!
//! Formato de texto simple (sin dependencias), la presentación usada más recientemente primero:
//!
//! ```text
//! [/home/ana/charla.pdf]
//! notas=/home/ana/charla-notas.pdf
//! desfase=1
//! opacidad=60
//! tinta=clara
//! monitor=DP-1
//! ventana=0
//! ```

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::render::Ink;

/// Máximo de presentaciones recordadas.
const MAX_ENTRIES: usize = 100;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub notes: Option<PathBuf>,
    pub offset: i32,
    /// Opacidad de las notas en porcentaje (5–100).
    pub opacity: u8,
    pub ink: Ink,
    pub monitor: Option<String>,
    pub windowed: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            notes: None,
            offset: 0,
            opacity: 60,
            ink: Ink::Dark,
            monitor: None,
            windowed: false,
        }
    }
}

impl Settings {
    /// Argumentos de `tela` para presentar `slides` con estos ajustes.
    pub fn to_args(&self, slides: &Path) -> Vec<OsString> {
        let mut a: Vec<OsString> = vec![slides.into()];
        if let Some(n) = &self.notes {
            a.extend(["--notas".into(), n.into()]);
        }
        a.extend(["--desfase".into(), self.offset.to_string().into()]);
        a.extend([
            "--opacidad".into(),
            self.opacity.clamp(5, 100).to_string().into(),
        ]);
        match self.ink {
            Ink::Dark => {}
            Ink::Light => a.push("--tinta-clara".into()),
            Ink::Off => a.push("--sin-tinta".into()),
        }
        if let Some(m) = &self.monitor {
            a.extend(["--monitor".into(), m.into()]);
        }
        if self.windowed {
            a.push("--ventana".into());
        }
        a
    }
}

#[derive(Default, Debug)]
pub struct Store {
    entries: Vec<(PathBuf, Settings)>,
}

pub fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(std::env::temp_dir)
        .join("tela")
        .join("presentaciones.conf")
}

impl Store {
    pub fn load() -> Store {
        std::fs::read_to_string(config_path())
            .map(|s| Store::parse(&s))
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Escritura atómica: primero a un temporal y luego se renombra.
        let tmp = path.with_extension("conf.tmp");
        std::fs::write(&tmp, self.serialize())?;
        std::fs::rename(tmp, path)
    }

    pub fn get(&self, slides: &Path) -> Option<&Settings> {
        self.entries
            .iter()
            .find(|(p, _)| p == slides)
            .map(|(_, s)| s)
    }

    /// Guarda los ajustes de `slides` y la marca como la más reciente.
    pub fn put(&mut self, slides: &Path, settings: Settings) {
        self.entries.retain(|(p, _)| p != slides);
        self.entries.insert(0, (slides.to_path_buf(), settings));
        self.entries.truncate(MAX_ENTRIES);
    }

    fn parse(text: &str) -> Store {
        let mut entries: Vec<(PathBuf, Settings)> = Vec::new();
        for line in text.lines() {
            let line = line.trim_end();
            if let Some(path) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                entries.push((PathBuf::from(path), Settings::default()));
                continue;
            }
            let (Some((_, s)), Some((key, value))) = (entries.last_mut(), line.split_once('='))
            else {
                continue;
            };
            match key.trim() {
                "notas" if !value.is_empty() => s.notes = Some(PathBuf::from(value)),
                "desfase" => s.offset = value.parse().unwrap_or(0),
                "opacidad" => {
                    s.opacity = value.parse::<u32>().map_or(60, |v| v.clamp(5, 100) as u8)
                }
                "tinta" => s.ink = Ink::from_id(value).unwrap_or(Ink::Dark),
                "monitor" if !value.is_empty() => s.monitor = Some(value.to_string()),
                "ventana" => s.windowed = value == "1",
                _ => {}
            }
        }
        Store { entries }
    }

    fn serialize(&self) -> String {
        let mut out = String::new();
        for (path, s) in &self.entries {
            // Rutas con saltos de línea no se pueden representar en este formato.
            let Some(p) = path.to_str().filter(|p| !p.contains('\n')) else {
                continue;
            };
            out.push_str(&format!("[{p}]\n"));
            if let Some(n) = s
                .notes
                .as_ref()
                .and_then(|n| n.to_str())
                .filter(|n| !n.contains('\n'))
            {
                out.push_str(&format!("notas={n}\n"));
            }
            out.push_str(&format!("desfase={}\n", s.offset));
            out.push_str(&format!("opacidad={}\n", s.opacity));
            out.push_str(&format!("tinta={}\n", s.ink.id()));
            if let Some(m) = s.monitor.as_ref().filter(|m| !m.contains('\n')) {
                out.push_str(&format!("monitor={m}\n"));
            }
            out.push_str(&format!("ventana={}\n\n", u8::from(s.windowed)));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{Parsed, parse_from};

    fn sample() -> Settings {
        Settings {
            notes: Some(PathBuf::from("/tmp/mis notas [v2].pdf")),
            offset: -1,
            opacity: 45,
            ink: Ink::Light,
            monitor: Some("DP-1".into()),
            windowed: true,
        }
    }

    #[test]
    fn roundtrip() {
        let mut store = Store::default();
        store.put(Path::new("/tmp/a.pdf"), Settings::default());
        store.put(Path::new("/tmp/charla [final].pptx"), sample());
        let parsed = Store::parse(&store.serialize());
        assert_eq!(
            parsed.get(Path::new("/tmp/charla [final].pptx")),
            Some(&sample())
        );
        assert_eq!(
            parsed.get(Path::new("/tmp/a.pdf")),
            Some(&Settings::default())
        );
        // La más reciente queda primero.
        assert_eq!(
            parsed.entries[0].0,
            PathBuf::from("/tmp/charla [final].pptx")
        );
    }

    #[test]
    fn put_replaces_existing() {
        let mut store = Store::default();
        store.put(Path::new("/x.pdf"), Settings::default());
        store.put(Path::new("/x.pdf"), sample());
        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.get(Path::new("/x.pdf")), Some(&sample()));
    }

    #[test]
    fn args_match_cli_parser() {
        let s = sample();
        let argv = s.to_args(Path::new("/tmp/charla.pdf"));
        let strings = argv.iter().map(|a| a.to_string_lossy().into_owned());
        let Ok(Parsed::Run(a)) = parse_from(strings) else {
            panic!("no se pudo analizar")
        };
        assert_eq!(a.slides, PathBuf::from("/tmp/charla.pdf"));
        assert_eq!(a.notes, s.notes);
        assert_eq!(a.offset, -1);
        assert!((a.opacity - 0.45).abs() < 1e-6);
        assert_eq!(a.ink, Ink::Light);
        assert_eq!(a.monitor.as_deref(), Some("DP-1"));
        assert!(a.windowed);
    }

    #[test]
    fn ignores_garbage() {
        let store = Store::parse("basura\nopacidad=999\n[/a.pdf]\nopacidad=999\ntinta=rara\n");
        let s = store.get(Path::new("/a.pdf")).unwrap();
        assert_eq!(s.opacity, 100);
        assert_eq!(s.ink, Ink::Dark);
    }
}
