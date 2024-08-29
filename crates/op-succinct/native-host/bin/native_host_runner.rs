use anyhow::Result;
use clap::Parser;
use kona_host::{init_tracing_subscriber, start_server, start_server_and_native_client, HostCli};
use log::{error, info};

// Source: https://github.com/ethereum-optimism/kona/blob/main/bin/host/src/main.rs
#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cfg = HostCli::parse();
    init_tracing_subscriber(cfg.v)?;

    if cfg.server {
        start_server(cfg).await?;
    } else {
        let res = start_server_and_native_client(cfg).await;
        if res.is_err() {
            error!("Failed to run server and native client: {:?}", res.err().unwrap());
            std::process::exit(1);
        }
    }

    info!("Exiting host program.");
    Ok(())
}
