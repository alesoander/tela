mod app;
mod args;
mod convert;
mod render;

use args::Parsed;

fn main() {
    // Los búferes de página (varios MB) se piden y liberan en cada render. Con un umbral de mmap
    // fijo, glibc los devuelve al sistema al liberarlos en vez de retenerlos en el heap.
    // SAFETY: se llama al inicio, antes de crear hilos ni reservar memoria relevante.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 512 * 1024);
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
