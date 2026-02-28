//! CLI definition for the TEE prover server.

use base_enclave_server::{ProverConfig, transport::TransportConfig};
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
    /// Run the Nitro enclave server.
    Nitro(NitroArgs),
    /// Run the HTTP-to-vsock proxy.
    Proxy(ProxyArgs),
}

/// Arguments for the nitro subcommand.
#[derive(Parser)]
struct NitroArgs {
    /// HTTP port to listen on.
    #[arg(long, env = "OP_ENCLAVE_HTTP_PORT", default_value_t = 1234)]
    http_port: u16,

    /// Vsock port to listen on.
    #[arg(long, env = "OP_ENCLAVE_VSOCK_PORT", default_value_t = 1234)]
    vsock_port: u32,

    /// Maximum HTTP body size in bytes.
    #[arg(long, env = "OP_ENCLAVE_HTTP_BODY_LIMIT", default_value_t = 52_428_800)]
    http_body_limit: u32,
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

impl TryFrom<Cli> for ProverConfig {
    type Error = eyre::Report;

    fn try_from(cli: Cli) -> Result<Self, Self::Error> {
        match cli.command {
            Command::Nitro(args) => {
                let config =
                    TransportConfig::new(args.vsock_port, args.http_port, args.http_body_limit);
                Ok(Self::Nitro(config))
            }
            Command::Proxy(args) => Ok(Self::Proxy {
                vsock_cid: args.vsock_cid,
                vsock_port: args.vsock_port,
                http_port: args.http_port,
            }),
        }
    }
}

impl Cli {
    /// Run the selected subcommand.
    pub(crate) async fn run(self) -> eyre::Result<()> {
        tracing_subscriber::fmt::init();
        let config = ProverConfig::try_from(self)?;
        config.run().await
    }
}
