//! Conversión de presentaciones (PPT/PPTX/ODP) a PDF con LibreOffice, con caché.
//!
//! La conversión ocurre una sola vez por archivo (se identifica por ruta, tamaño y fecha de
//! modificación); después se reutiliza el PDF de `~/.cache/tela/`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const OFFICE_EXTS: &[&str] = &["ppt", "pptx", "pps", "ppsx", "pptm", "odp", "key"];

/// Devuelve la ruta a un PDF equivalente a `path` (convirtiéndolo si hace falta).
pub fn ensure_pdf(path: &Path) -> Result<PathBuf, String> {
    let path = path
        .canonicalize()
        .map_err(|e| format!("no se puede abrir «{}»: {e}", path.display()))?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    if ext == "pdf" {
        return Ok(path);
    }
    if !OFFICE_EXTS.contains(&ext.as_str()) {
        return Err(format!(
            "formato no soportado: «{}» (usa PDF o PPT/PPTX/ODP)",
            path.display()
        ));
    }
    convert_with_libreoffice(&path)
}

fn cache_root() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("tela")
}

fn convert_with_libreoffice(path: &Path) -> Result<PathBuf, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    meta.len().hash(&mut h);
    meta.modified().ok().hash(&mut h);

    let root = cache_root();
    let out_dir = root.join(format!("{:016x}", h.finish()));
    let stem = path.file_stem().unwrap_or_default();
    let out = out_dir.join(stem).with_extension("pdf");
    if out.is_file() {
        return Ok(out);
    }

    std::fs::create_dir_all(&out_dir)
        .map_err(|e| format!("no se puede crear {}: {e}", out_dir.display()))?;

    eprintln!(
        "tela: convirtiendo «{}» a PDF con LibreOffice (solo la primera vez)…",
        path.file_name().unwrap_or_default().to_string_lossy()
    );

    // Perfil propio para no chocar con un LibreOffice que el usuario tenga abierto.
    let profile = glib::filename_to_uri(root.join("lo-perfil"), None)
        .map_err(|e| format!("ruta de perfil inválida: {e}"))?;

    let mut last_err = None;
    for bin in ["soffice", "libreoffice"] {
        let result = Command::new(bin)
            .arg("--headless")
            .arg("--norestore")
            .arg("--nologo")
            .arg(format!("-env:UserInstallation={profile}"))
            .args(["--convert-to", "pdf", "--outdir"])
            .arg(&out_dir)
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output();
        match result {
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("no se pudo ejecutar {bin}: {e}")),
            Ok(o) => {
                if out.is_file() {
                    return Ok(out);
                }
                last_err = Some(String::from_utf8_lossy(&o.stderr).trim().to_string());
                break;
            }
        }
    }

    match last_err {
        None => Err("no se encontró LibreOffice para convertir el PowerPoint.\n\
             Instálalo con `sudo pacman -S libreoffice-fresh` o exporta tu presentación a PDF."
            .into()),
        Some(msg) => Err(format!(
            "LibreOffice no pudo convertir «{}». {msg}",
            path.display()
        )),
    }
}
