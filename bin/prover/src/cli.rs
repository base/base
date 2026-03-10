//! CLI definition for the TEE prover binary.

use std::{net::SocketAddr, sync::Arc};

use alloy_primitives::{Address, B256};
use base_consensus_registry::Registry;
use base_proof_host::ProverConfig;
#[cfg(target_os = "linux")]
use base_proof_tee_nitro::{NitroEnclave, EnclaveConfig};
use base_proof_tee_nitro::NitroProverServer;
#[cfg(feature = "local")]
use base_proof_tee_nitro::Server;
#[cfg(feature = "local")]
use base_proof_tee_nitro::EnclaveConfig;
use base_proof_transport::NativeTransport;
use base_proof_transport::VsockTransport;
use clap::{Parser, Subcommand};
use eyre::eyre;
use tracing::info;

/// TEE prover.
#[derive(Parser)]
#[command(author, version)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Proving backend.
#[derive(Subcommand)]
enum Command {
    /// AWS Nitro Enclave proving backend.
    Nitro(NitroArgs),
}

/// Arguments for the `nitro` subcommand.
#[derive(Parser)]
struct NitroArgs {
    #[command(subcommand)]
    command: NitroCommand,
}

/// Nitro subcommands.
#[derive(Subcommand)]
enum NitroCommand {
    /// Run the JSON-RPC server on the EC2 host.
    ///
    /// Accepts proving requests over JSON-RPC and forwards them to the Nitro
    /// Enclave over vsock.
    Server(NitroServerArgs),

    /// Run the proving process inside the Nitro Enclave.
    ///
    /// Listens on vsock for proving requests from the host server.
    Enclave(NitroEnclaveArgs),

    /// Run server and enclave in a single process for local development.
    #[cfg(feature = "local")]
    Local(NitroLocalArgs),
}

/// Arguments for the `nitro server` subcommand.
#[derive(Parser)]
struct NitroServerArgs {
    /// L1 execution layer RPC URL.
    #[arg(long, env = "L1_ETH_URL")]
    l1_eth_url: String,

    /// L2 execution layer RPC URL.
    #[arg(long, env = "L2_ETH_URL")]
    l2_eth_url: String,

    /// L1 beacon API URL.
    #[arg(long, env = "L1_BEACON_URL")]
    l1_beacon_url: String,

    /// L2 chain ID.
    #[arg(long, env = "L2_CHAIN_ID")]
    l2_chain_id: u64,

    /// Socket address to listen on for JSON-RPC.
    #[arg(long, env = "RPC_ADDR", default_value = "0.0.0.0:7300")]
    rpc_addr: SocketAddr,

    /// Vsock CID of the enclave.
    #[arg(long, env = "VSOCK_CID", default_value_t = 16)]
    vsock_cid: u32,

    /// Vsock port to connect to the enclave.
    #[arg(long, env = "VSOCK_PORT", default_value_t = 1234)]
    vsock_port: u32,

    /// Enable experimental `debug_executePayload` witness endpoint.
    #[arg(long, env = "ENABLE_EXPERIMENTAL_WITNESS_ENDPOINT")]
    enable_experimental_witness_endpoint: bool,
}

/// Arguments for the `nitro enclave` subcommand.
#[derive(Parser)]
struct NitroEnclaveArgs {
    /// Vsock port to listen on.
    #[arg(long, env = "VSOCK_PORT", default_value_t = 1234)]
    vsock_port: u32,

    /// Proposer address.
    #[arg(long, env = "PROPOSER")]
    proposer: Address,

    /// Per-chain configuration hash.
    #[arg(long, env = "CONFIG_HASH")]
    config_hash: B256,

    /// Expected PCR0 measurement of the enclave image.
    #[arg(long, env = "TEE_IMAGE_HASH")]
    tee_image_hash: B256,
}

impl Cli {
    /// Run the selected subcommand.
    pub(crate) async fn run(self) -> eyre::Result<()> {
        tracing_subscriber::fmt::init();
        match self.command {
            Command::Nitro(args) => args.run().await,
        }
    }
}

impl NitroArgs {
    async fn run(self) -> eyre::Result<()> {
        match self.command {
            NitroCommand::Server(args) => args.run().await,
            NitroCommand::Enclave(args) => args.run().await,
            #[cfg(feature = "local")]
            NitroCommand::Local(args) => args.run().await,
        }
    }
}

impl NitroServerArgs {
    async fn run(self) -> eyre::Result<()> {
        let rollup_config = Registry::rollup_config(self.l2_chain_id)
            .ok_or_else(|| eyre!("unknown L2 chain ID: {}", self.l2_chain_id))?
            .clone();

        let l1_config = Registry::l1_config(rollup_config.l1_chain_id)
            .ok_or_else(|| eyre!("unknown L1 chain ID: {}", rollup_config.l1_chain_id))?
            .clone();

        let config = ProverConfig {
            l1_eth_url: self.l1_eth_url,
            l2_eth_url: self.l2_eth_url,
            l1_beacon_url: self.l1_beacon_url,
            l2_chain_id: self.l2_chain_id,
            rollup_config,
            l1_config,
            enable_experimental_witness_endpoint: self.enable_experimental_witness_endpoint,
        };

        let transport = Arc::new(VsockTransport::new(self.vsock_cid, self.vsock_port));
        let server = NitroProverServer::new(config, transport);

        info!(addr = %self.rpc_addr, "starting nitro prover server");
        let handle = server.run(self.rpc_addr).await?;
        handle.stopped().await;
        Ok(())
    }
}

impl NitroEnclaveArgs {
    async fn run(self) -> eyre::Result<()> {
        #[cfg(not(target_os = "linux"))]
        return Err(eyre!("enclave subcommand is only supported on Linux"));

        #[cfg(target_os = "linux")]
        {
            let config = EnclaveConfig {
                vsock_port: self.vsock_port,
                proposer: self.proposer,
                config_hash: self.config_hash,
                tee_image_hash: self.tee_image_hash,
            };

            let enclave = NitroEnclave::new(&config)?;
            enclave.run().await
        }
    }
}

/// Arguments for the `nitro local` subcommand.
#[cfg(feature = "local")]
#[derive(Parser)]
struct NitroLocalArgs {
    /// L1 execution layer RPC URL.
    #[arg(long, env = "L1_ETH_URL")]
    l1_eth_url: String,

    /// L2 execution layer RPC URL.
    #[arg(long, env = "L2_ETH_URL")]
    l2_eth_url: String,

    /// L1 beacon API URL.
    #[arg(long, env = "L1_BEACON_URL")]
    l1_beacon_url: String,

    /// L2 chain ID.
    #[arg(long, env = "L2_CHAIN_ID")]
    l2_chain_id: u64,

    /// Socket address to listen on for JSON-RPC.
    #[arg(long, env = "RPC_ADDR", default_value = "0.0.0.0:7300")]
    rpc_addr: SocketAddr,

    /// Proposer address.
    #[arg(long, env = "PROPOSER")]
    proposer: Address,

    /// Per-chain configuration hash.
    #[arg(long, env = "CONFIG_HASH")]
    config_hash: B256,

    /// Expected PCR0 measurement of the enclave image.
    #[arg(long, env = "TEE_IMAGE_HASH")]
    tee_image_hash: B256,

    /// Enable experimental `debug_executePayload` witness endpoint.
    #[arg(long, env = "ENABLE_EXPERIMENTAL_WITNESS_ENDPOINT")]
    enable_experimental_witness_endpoint: bool,
}

#[cfg(feature = "local")]
impl NitroLocalArgs {
    async fn run(self) -> eyre::Result<()> {
        let rollup_config = Registry::rollup_config(self.l2_chain_id)
            .ok_or_else(|| eyre!("unknown L2 chain ID: {}", self.l2_chain_id))?
            .clone();

        let l1_config = Registry::l1_config(rollup_config.l1_chain_id)
            .ok_or_else(|| eyre!("unknown L1 chain ID: {}", rollup_config.l1_chain_id))?
            .clone();

        let enclave_config = EnclaveConfig {
            vsock_port: 0,
            proposer: self.proposer,
            config_hash: self.config_hash,
            tee_image_hash: self.tee_image_hash,
        };

        let enclave_server = Arc::new(Server::new(&enclave_config)?);
        let transport = Arc::new(NativeTransport::new({
            let server = Arc::clone(&enclave_server);
            move |preimages| {
                let server = Arc::clone(&server);
                let preimages = preimages.to_vec();
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(server.prove(preimages))
                        .expect("enclave prove failed")
                })
            }
        }));

        let prover_config = ProverConfig {
            l1_eth_url: self.l1_eth_url,
            l2_eth_url: self.l2_eth_url,
            l1_beacon_url: self.l1_beacon_url,
            l2_chain_id: self.l2_chain_id,
            rollup_config,
            l1_config,
            enable_experimental_witness_endpoint: self.enable_experimental_witness_endpoint,
        };

        let server = NitroProverServer::new(prover_config, transport);

        info!(addr = %self.rpc_addr, "starting nitro prover server (local mode)");
        let handle = server.run(self.rpc_addr).await?;
        handle.stopped().await;
        Ok(())
    }
}
