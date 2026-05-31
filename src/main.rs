//! The `redo-msh` umbrella binary: dispatch on a leading verb, falling back to
//! building the arguments as targets. The individually named commands (`redo`,
//! `redo-ifchange`, ...) live in `src/bin/` and forward into the library.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match redo_msh::run(&args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("redo-msh: {e:#}");
            1
        }
    };
    std::process::exit(code);
}
