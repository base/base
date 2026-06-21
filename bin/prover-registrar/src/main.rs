#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use clap::Parser as _;

mod cli;

#[tokio::main]
async fn main() {
    let result: eyre::Result<()> = async {
        let config = cli::Cli::parse().config()?;
        config.run().await?;
        Ok(())
    }
    .await;

    if let Err(err) = result {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
