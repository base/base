use clap::Parser;
use kona_host::HostCli;
use native_host::run_native_host;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cfg = HostCli::parse();
    run_native_host(&cfg).await.unwrap();
}
