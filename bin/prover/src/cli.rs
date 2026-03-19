//! CLI definition for the prover binary (TEE + ZK backends).

use std::net::SocketAddr;
#[cfg(any(target_os = "linux", feature = "local"))]
use std::sync::Arc;

use base_cli_utils::{LogConfig, RuntimeManager};
#[cfg(any(target_os = "linux", feature = "local"))]
use base_consensus_registry::Registry;
#[cfg(any(target_os = "linux", feature = "local"))]
use base_proof_host::ProverConfig;
#[cfg(feature = "local")]
use base_proof_tee_nitro::Server;
#[cfg(target_os = "linux")]
use base_proof_tee_nitro::{NitroEnclave, VSOCK_PORT};
#[cfg(any(target_os = "linux", feature = "local"))]
use base_proof_tee_nitro::{NitroProverServer, NitroTransport};
use clap::{Parser, Subcommand};
#[cfg(any(target_os = "linux", feature = "local"))]
use eyre::eyre;
#[cfg(any(target_os = "linux", feature = "local"))]
use tracing::info;

use crate::zk;

base_cli_utils::define_log_args!("BASE_PROVER");
base_cli_utils::define_metrics_args!("BASE_PROVER", 7300);

/// Prover binary (TEE + ZK backends).
#[derive(Parser)]
#[command(author, version)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Logging arguments.
    #[command(flatten)]
    logging: LogArgs,

    /// Metrics arguments.
    #[command(flatten)]
    metrics: MetricsArgs,
}

/// Proving backend.
#[derive(Subcommand)]
enum Command {
    /// AWS Nitro Enclave proving backend.
    Nitro(NitroArgs),

    /// ZK prover service.
    Zk(Box<zk::ZkArgs>),
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
    #[cfg(target_os = "linux")]
    Server(NitroServerArgs),

    /// Run the proving process inside the Nitro Enclave.
    ///
    /// Listens on vsock for proving requests from the host server.
    #[cfg(target_os = "linux")]
    Enclave,

    /// Run server and enclave in a single process for local development.
    #[cfg(feature = "local")]
    Local(NitroLocalArgs),
}

/// Shared arguments for subcommands that run the JSON-RPC prover server.
#[derive(Parser)]
struct ProverServerArgs {
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
    #[arg(long, env = "LISTEN_ADDR")]
    listen_addr: SocketAddr,

    /// Enable experimental `debug_executePayload` witness endpoint.
    #[arg(long, env = "ENABLE_EXPERIMENTAL_WITNESS_ENDPOINT")]
    enable_experimental_witness_endpoint: bool,
}

/// Arguments for the `nitro server` subcommand.
#[cfg(target_os = "linux")]
#[derive(Parser)]
struct NitroServerArgs {
    #[command(flatten)]
    server: ProverServerArgs,

    /// Vsock CID of the enclave.
    #[arg(long, env = "VSOCK_CID")]
    vsock_cid: u32,
}

impl Cli {
    /// Run the selected subcommand.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let Self { command, logging, metrics } = self;
        LogConfig::from(logging).init_tracing_subscriber()?;
        base_cli_utils::MetricsConfig::from(metrics).init_with(|| {
            base_proof_host::Metrics::init();
            base_cli_utils::register_version_metrics!();
        })?;
        RuntimeManager::run_until_ctrl_c(async move {
            match command {
                Command::Nitro(args) => args.run().await,
                Command::Zk(args) => (*args).run().await,
            }
        })
    }
}

impl NitroArgs {
    async fn run(self) -> eyre::Result<()> {
        match self.command {
            #[cfg(target_os = "linux")]
            NitroCommand::Server(args) => args.run().await,
            #[cfg(target_os = "linux")]
            NitroCommand::Enclave => NitroEnclave::new()?.run().await,
            #[cfg(feature = "local")]
            NitroCommand::Local(args) => args.run().await,
        }
    }
}

#[cfg(target_os = "linux")]
impl NitroServerArgs {
    async fn run(self) -> eyre::Result<()> {
        let rollup_config = Registry::rollup_config(self.server.l2_chain_id)
            .ok_or_else(|| eyre!("unknown L2 chain ID: {}", self.server.l2_chain_id))?
            .clone();

        let l1_config = Registry::l1_config(rollup_config.l1_chain_id)
            .ok_or_else(|| eyre!("unknown L1 chain ID: {}", rollup_config.l1_chain_id))?
            .clone();

        let config = ProverConfig {
            l1_eth_url: self.server.l1_eth_url,
            l2_eth_url: self.server.l2_eth_url,
            l1_beacon_url: self.server.l1_beacon_url,
            l2_chain_id: self.server.l2_chain_id,
            rollup_config,
            l1_config,
            enable_experimental_witness_endpoint: self.server.enable_experimental_witness_endpoint,
        };

        let transport = Arc::new(NitroTransport::vsock(self.vsock_cid, VSOCK_PORT));
        let server = NitroProverServer::new(config, transport);

        info!(addr = %self.server.listen_addr, "starting nitro prover server");
        let handle = server.run(self.server.listen_addr).await?;
        handle.stopped().await;
        Ok(())
    }
}

/// Arguments for the `nitro local` subcommand.
#[cfg(feature = "local")]
#[derive(Parser)]
struct NitroLocalArgs {
    #[command(flatten)]
    server: ProverServerArgs,
}

#[cfg(feature = "local")]
impl NitroLocalArgs {
    async fn run(self) -> eyre::Result<()> {
        let rollup_config = Registry::rollup_config(self.server.l2_chain_id)
            .ok_or_else(|| eyre!("unknown L2 chain ID: {}", self.server.l2_chain_id))?
            .clone();

        let l1_config = Registry::l1_config(rollup_config.l1_chain_id)
            .ok_or_else(|| eyre!("unknown L1 chain ID: {}", rollup_config.l1_chain_id))?
            .clone();

        let prover_config = ProverConfig {
            l1_eth_url: self.server.l1_eth_url,
            l2_eth_url: self.server.l2_eth_url,
            l1_beacon_url: self.server.l1_beacon_url,
            l2_chain_id: self.server.l2_chain_id,
            rollup_config,
            l1_config,
            enable_experimental_witness_endpoint: self.server.enable_experimental_witness_endpoint,
        };

        let enclave_server = Arc::new(Server::new_local()?);
        let transport = Arc::new(NitroTransport::local(enclave_server));
        let server = NitroProverServer::new(prover_config, transport);

        info!(addr = %self.server.listen_addr, "starting nitro prover server (local mode)");
        let handle = server.run(self.server.listen_addr).await?;
        handle.stopped().await;
        Ok(())
    }
}
