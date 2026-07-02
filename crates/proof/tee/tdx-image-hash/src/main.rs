#![doc = include_str!("../README.md")]

use base_proof_tee_tdx_image_hash::{Cli, TdxImageHashTool};
use clap::Parser as _;

#[tokio::main]
async fn main() {
    match TdxImageHashTool::run(Cli::parse().config()).await {
        Ok(report) => println!("{report}"),
        Err(error) => {
            eprintln!("Error: {error:?}");
            std::process::exit(1);
        }
    }
}
