//! CLI definition for the TEE prover server.

use clap::{Parser, Subcommand};

/// TEE prover.
#[derive(Parser)]
#[command(author, version)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Available subcommands.
#[derive(Subcommand)]
enum Command {
    /// Run the HTTP-to-vsock proxy.
    Proxy(ProxyArgs),
}

/// Arguments for the proxy subcommand.
#[derive(Parser)]
struct ProxyArgs {
    /// Vsock CID of the enclave.
    #[arg(long, env = "VSOCK_CID", default_value_t = 16)]
    vsock_cid: u32,

    /// Vsock port to connect to.
    #[arg(long, env = "VSOCK_PORT", default_value_t = 1234)]
    vsock_port: u32,

    /// HTTP port to listen on.
    #[arg(long, env = "HTTP_PORT", default_value_t = 7333)]
    http_port: u16,
}

impl Cli {
    /// Run the selected subcommand.
    pub(crate) async fn run(self) -> eyre::Result<()> {
        tracing_subscriber::fmt::init();
        match self.command {
            Command::Proxy(args) => {
                base_enclave_server::run_proxy(args.vsock_cid, args.vsock_port, args.http_port)
                    .await
            }
        }
    }
}
