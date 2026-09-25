#![doc = include_str!("../README.md")]

use base_acceptance::{AcceptanceCli, ExitCode};
use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = AcceptanceCli::parse();
    let code = match cli.execute().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error:?}");
            ExitCode::Config
        }
    };

    std::process::exit(code as i32);
}
