//! Renders the native event given as the first argument, field by field:
//! `cargo run -p kerita --example annotate -- -FAu0OKERICAACAAXicp…`.

use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(event) = std::env::args().nth(1) else {
        let _ = writeln!(std::io::stderr(), "usage: annotate <native event>");
        return ExitCode::FAILURE;
    };
    let rendered = kerita::annotate::annotate(event.as_bytes());
    match writeln!(std::io::stdout(), "{rendered}") {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
