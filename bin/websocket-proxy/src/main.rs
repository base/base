//! Websocket proxy binary entry point.

mod cli;

use base_cli_utils::LogConfig;
use clap::Parser;
use cli::Args;
use dotenvy::dotenv;
use websocket_proxy::WebsocketProxyRunner;

base_cli_utils::define_log_args!("WEBSOCKET_PROXY");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    let args = Args::parse();

    LogConfig::from(args.log.clone()).init_tracing_subscriber().expect("failed to initialize tracing");

    let config = args.try_into()?;

    WebsocketProxyRunner::run(config).await
}
