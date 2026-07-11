//! Extract plain text from a `.xls` file.
//!
//! ```text
//! cargo run -p rxls --example extract -- path/to/book.xls
//! ```

use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: extract <file.xls>");
        return ExitCode::from(64);
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {path}: {e}");
            return ExitCode::from(66);
        }
    };
    match rxls::extract_text(&bytes) {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{path}: {e}");
            ExitCode::from(1)
        }
    }
}
