use kona_host::{start_server_and_native_client, HostCli, init_tracing_subscriber};
use clap::Parser;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cfg = HostCli::parse();
    init_tracing_subscriber(cfg.v).unwrap();
    start_server_and_native_client(cfg).await.unwrap();
}
