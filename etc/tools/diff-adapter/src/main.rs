//! `base-diff-adapter`: answers differential execution requests on stdin (see `PROTOCOL.md`).

use std::{
    io::{self, BufRead, Write},
    process::ExitCode,
};

use base_diff_adapter::DiffAdapter;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();

    let adapter = DiffAdapter::base_mainnet();
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) => return ExitCode::SUCCESS,
            Ok(_) => {}
            Err(error) => {
                tracing::error!(error = %error, "reading stdin failed");
                return ExitCode::FAILURE;
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        let response = adapter.handle_line(&line);
        let written = serde_json::to_writer(&mut stdout, &response)
            .map_err(io::Error::from)
            .and_then(|()| stdout.write_all(b"\n"))
            .and_then(|()| stdout.flush());
        if let Err(error) = written {
            tracing::error!(error = %error, "writing stdout failed");
            return ExitCode::FAILURE;
        }
    }
}
