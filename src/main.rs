use std::io::Write;
use std::path::PathBuf;

fn main() {
    let mut data = PathBuf::from("/data");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--data-dir" {
            let Some(path) = args.next() else {
                std::process::exit(2);
            };
            data = PathBuf::from(path);
        } else {
            let _ = writeln!(std::io::stderr(), "unknown argument: {arg}");
            std::process::exit(2);
        }
    }
    if let Err(error) = minivectordb::run(&data) {
        let _ = writeln!(std::io::stderr(), "{error}");
        std::process::exit(1);
    }
}
