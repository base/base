use clap::Parser;
use kona_host::{init_tracing_subscriber, start_server_and_native_client, HostCli};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cfg = HostCli::parse();
    init_tracing_subscriber(cfg.v).unwrap();
    start_server_and_native_client(cfg.clone()).await.unwrap();
}
