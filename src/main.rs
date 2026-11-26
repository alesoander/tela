use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use tela::args::{self, Parsed};
use tela::{app, convert};

fn main() {
    // Los búferes de página (varios MB) se piden y liberan en cada render. Con un umbral de mmap
    // fijo, glibc los devuelve al sistema al liberarlos en vez de retenerlos en el heap.
    // SAFETY: se llama al inicio, antes de crear hilos ni reservar memoria relevante.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 512 * 1024);
    }

    // Sin argumentos se abre el lanzador gráfico.
    if std::env::args_os().len() == 1 {
        open_launcher();
    }

    let args = match args::parse() {
        Ok(Parsed::Run(a)) => a,
        Ok(Parsed::Help) => {
            print!("{}", args::HELP);
            return;
        }
        Err(msg) => {
            eprintln!("tela: {msg}");
            std::process::exit(2);
        }
    };

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let slides = convert::ensure_pdf(&args.slides)?;
        let notes = match &args.notes {
            Some(n) => Some(convert::ensure_pdf(n)?),
            None => {
                eprintln!("tela: sin archivo de notas (usa --notas <archivo>)");
                None
            }
        };
        app::run(&args, slides, notes)
    })();

    if let Err(e) = result {
        eprintln!("tela: {e}");
        std::process::exit(1);
    }
}

/// Reemplaza este proceso por `tela-gui` (junto al ejecutable actual o en el PATH).
fn open_launcher() -> ! {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| Some(p.parent()?.join("tela-gui")))
        .filter(|p| p.is_file());
    let gui = sibling.unwrap_or_else(|| PathBuf::from("tela-gui"));
    let err = Command::new(&gui).exec();
    eprintln!(
        "tela: no se pudo abrir el lanzador ({}): {err}\n",
        gui.display()
    );
    print!("{}", args::HELP);
    std::process::exit(1);
}
