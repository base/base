//! `base-diff-trie-adapter`: answers differential trie requests on stdin (see
//! `TRIE_PROTOCOL.md`).

use std::{
    io::{self, BufRead, Write},
    process::ExitCode,
};

use base_diff_trie_adapter::TrieAdapter;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error")),
        )
        .init();

    // The adapter catches panics of the trie code and reports them in its responses.
    std::panic::set_hook(Box::new(|info| tracing::debug!(panic = %info, "caught panic")));
    let adapter = TrieAdapter::new();
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
