//! CLI / env configuration for the Roxy HTTP server.

use std::net::SocketAddr;

use clap::Args;

/// Configuration for the Roxy HTTP server.
#[derive(Args, Debug, Clone)]
pub struct Config {
    /// Socket address to bind the HTTP server to.
    #[arg(long, env = "ROXY_LISTEN_ADDR", default_value = "0.0.0.0:8545")]
    pub listen_addr: SocketAddr,
}
