//! CLI definition for the TEE prover server.

use base_enclave_server::transport::TransportConfig;
use base_proof_host::{Host, HostArgs};
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
    /// Run the preimage oracle host server.
    Host(Box<HostArgs>),
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

impl Cli {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        tracing_subscriber::fmt::init();
        match self.command {
            Command::Nitro(args) => {
                let config =
                    TransportConfig::new(args.vsock_port, args.http_port, args.http_body_limit);
                base_enclave_server::run_enclave(config).await
            }
            Command::Proxy(args) => {
                base_enclave_server::run_proxy(args.vsock_cid, args.vsock_port, args.http_port)
                    .await
            }
            Command::Host(args) => Ok(Host::new(*args).start().await?),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use base_proof_host::HostArgs;
    use clap::Parser;

    #[test]
    fn test_host_cli_flags() {
        let zero_hash = B256::ZERO;
        let args = HostArgs::try_parse_from([
            "host",
            "--l1-head",
            zero_hash.to_string().as_str(),
            "--agreed-l2-head-hash",
            zero_hash.to_string().as_str(),
            "--agreed-l2-output-root",
            zero_hash.to_string().as_str(),
            "--claimed-l2-output-root",
            zero_hash.to_string().as_str(),
            "--claimed-l2-block-number",
            "0",
        ])
        .unwrap();
        assert_eq!(args.l1_head, zero_hash);
        assert_eq!(args.agreed_l2_head_hash, zero_hash);
        assert_eq!(args.agreed_l2_output_root, zero_hash);
        assert_eq!(args.claimed_l2_output_root, zero_hash);
        assert_eq!(args.claimed_l2_block_number, 0);
    }
}
