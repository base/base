use anyhow::Result;
use kona_host::{init_tracing_subscriber, start_server_and_native_client, HostCli};

pub async fn run_native_host(cfg: &HostCli) -> Result<()> {
    init_tracing_subscriber(cfg.v).unwrap();
    start_server_and_native_client(cfg.clone()).await.unwrap();
    Ok(())
}
