//! The tiger client binary.

use std::process::ExitCode;

const USAGE: &str = "usage: tiger diff-adapter";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("diff-adapter") => match base_tiger::Adapter::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("tiger diff-adapter: {error}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}
