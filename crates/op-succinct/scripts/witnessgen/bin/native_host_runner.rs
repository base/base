use anyhow::Result;
use clap::Parser;
use kona_host::{init_tracing_subscriber, start_server, start_server_and_native_client, HostCli};

// Source: https://github.com/ethereum-optimism/kona/blob/main/bin/host/src/main.rs
#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cfg = HostCli::parse();
    init_tracing_subscriber(cfg.v)?;

    if cfg.server {
        let res = start_server(cfg.clone()).await;
        if res.is_err() {
            std::process::exit(1);
        }
    } else {
        let res = start_server_and_native_client(cfg.clone()).await;
        if res.is_err() {
            std::process::exit(1);
        }
    }

    println!(
        "Ran host program with end block: {:?}",
        cfg.claimed_l2_block_number
    );
    std::process::exit(0);
}
