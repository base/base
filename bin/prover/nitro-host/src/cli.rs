//! CLI definition for the Nitro TEE prover host binary.

use std::net::SocketAddr;
#[cfg(any(target_os = "linux", feature = "local"))]
use std::sync::Arc;
#[cfg(any(target_os = "linux", feature = "local"))]
use std::time::Duration;

use alloy_primitives::Address;
use base_cli_utils::{LogConfig, RuntimeManager};
#[cfg(any(target_os = "linux", feature = "local"))]
use base_consensus_registry::Registry;
#[cfg(any(target_os = "linux", feature = "local"))]
use base_proof_host::ProverConfig;
#[cfg(feature = "local")]
use base_proof_tee_nitro_enclave::Server as EnclaveServer;
#[cfg(target_os = "linux")]
use base_proof_tee_nitro_enclave::VSOCK_PORT;
#[cfg(any(target_os = "linux", feature = "local"))]
use base_proof_tee_nitro_host::RegistrationHealthConfig;
#[cfg(any(target_os = "linux", feature = "local"))]
use base_proof_tee_nitro_host::{NitroProverServer, NitroTransport};
use clap::{Parser, Subcommand};
#[cfg(any(target_os = "linux", feature = "local"))]
use eyre::eyre;
#[cfg(any(target_os = "linux", feature = "local"))]
use tracing::info;
#[cfg(feature = "local")]
use tracing::warn;

base_cli_utils::define_log_args!("BASE_PROVER_NITRO_HOST");
base_cli_utils::define_metrics_args!("BASE_PROVER_NITRO_HOST", 7300);

/// Nitro TEE prover host binary.
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

/// Nitro host subcommands.
#[derive(Subcommand)]
enum Command {
    /// Run the JSON-RPC server on the EC2 host.
    ///
    /// Accepts proving requests over JSON-RPC and forwards them to the Nitro
    /// Enclave over vsock.
    #[cfg(target_os = "linux")]
    Server(ServerArgs),

    /// Run server and enclave in a single process for local development.
    #[cfg(feature = "local")]
    Local(LocalArgs),
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

    /// Maximum seconds for a single proof request before it is aborted.
    #[arg(long, env = "PROOF_REQUEST_TIMEOUT_SECS", default_value = "1740", value_parser = clap::value_parser!(u64).range(1..))]
    proof_request_timeout_secs: u64,

    /// `TEEProverRegistry` contract address on L1. When set, `/healthz` returns
    /// healthy only if the enclave signer is registered on-chain.
    #[arg(long, env = "TEE_PROVER_REGISTRY_ADDRESS")]
    tee_prover_registry_address: Option<Address>,
}

#[cfg(any(target_os = "linux", feature = "local"))]
impl ProverServerArgs {
    fn registration_health_config(&self) -> Option<RegistrationHealthConfig> {
        self.tee_prover_registry_address.map(|address| RegistrationHealthConfig {
            registry_address: address,
            l1_rpc_url: self.l1_eth_url.clone(),
        })
    }
}

/// Arguments for the `server` subcommand.
#[cfg(target_os = "linux")]
#[derive(Parser)]
struct ServerArgs {
    #[command(flatten)]
    server: ProverServerArgs,

    /// Vsock CID(s) of the enclave(s), comma-separated for multi-enclave mode.
    #[arg(long, env = "VSOCK_CID", value_delimiter = ',')]
    vsock_cid: Vec<u32>,
}

impl Cli {
    /// Run the selected subcommand.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let Self { command, logging, metrics } = self;
        LogConfig::from(logging).init_tracing_subscriber()?;
        base_cli_utils::MetricsConfig::from(metrics).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;
        RuntimeManager::new().with_thread_stack_size(8 * 1024 * 1024).run_until_ctrl_c(async move {
            match command {
                #[cfg(target_os = "linux")]
                Command::Server(args) => args.run().await,
                #[cfg(feature = "local")]
                Command::Local(args) => args.run().await,
            }
        })
    }
}

#[cfg(target_os = "linux")]
impl ServerArgs {
    async fn run(self) -> eyre::Result<()> {
        let rollup_config = Registry::rollup_config(self.server.l2_chain_id)
            .ok_or_else(|| eyre!("unknown L2 chain ID: {}", self.server.l2_chain_id))?
            .clone();

        let l1_config = Registry::l1_config(rollup_config.l1_chain_id)
            .ok_or_else(|| eyre!("unknown L1 chain ID: {}", rollup_config.l1_chain_id))?
            .clone();

        let registration_health = self.server.registration_health_config();

        let config = ProverConfig {
            l1_eth_url: self.server.l1_eth_url,
            l2_eth_url: self.server.l2_eth_url,
            l1_beacon_url: self.server.l1_beacon_url,
            l2_chain_id: self.server.l2_chain_id,
            rollup_config,
            l1_config,
            enable_experimental_witness_endpoint: self.server.enable_experimental_witness_endpoint,
        };

        if self.vsock_cid.is_empty() {
            return Err(eyre!("at least one --vsock-cid is required"));
        }
        if self.vsock_cid.len() > 1 && self.server.tee_prover_registry_address.is_none() {
            return Err(eyre!(
                "multi-CID requires --tee-prover-registry-address for on-chain routing"
            ));
        }
        let transports: Vec<Arc<NitroTransport>> = self
            .vsock_cid
            .iter()
            .map(|&cid| Arc::new(NitroTransport::vsock(cid, VSOCK_PORT)))
            .collect();
        let timeout = Duration::from_secs(self.server.proof_request_timeout_secs);
        let mut server = NitroProverServer::new_multi(config, transports, timeout);
        if let Some(reg) = registration_health {
            server = server.with_registration_health(reg);
        }

        info!(cids = ?self.vsock_cid, "configured vsock CIDs");
        info!(addr = %self.server.listen_addr, "starting nitro prover host server");
        let handle = server.run(self.server.listen_addr).await?;
        handle.stopped().await;
        Ok(())
    }
}

/// Arguments for the `local` subcommand.
#[cfg(feature = "local")]
#[derive(Parser)]
struct LocalArgs {
    #[command(flatten)]
    server: ProverServerArgs,

    /// Number of local enclave instances to run (minimum 1).
    #[arg(long, env = "LOCAL_ENCLAVE_COUNT", default_value = "1")]
    local_enclave_count: usize,
}

#[cfg(feature = "local")]
impl LocalArgs {
    async fn run(self) -> eyre::Result<()> {
        let rollup_config = Registry::rollup_config(self.server.l2_chain_id)
            .ok_or_else(|| eyre!("unknown L2 chain ID: {}", self.server.l2_chain_id))?
            .clone();

        let l1_config = Registry::l1_config(rollup_config.l1_chain_id)
            .ok_or_else(|| eyre!("unknown L1 chain ID: {}", rollup_config.l1_chain_id))?
            .clone();

        let registration_health = self.server.registration_health_config();

        let prover_config = ProverConfig {
            l1_eth_url: self.server.l1_eth_url,
            l2_eth_url: self.server.l2_eth_url,
            l1_beacon_url: self.server.l1_beacon_url,
            l2_chain_id: self.server.l2_chain_id,
            rollup_config,
            l1_config,
            enable_experimental_witness_endpoint: self.server.enable_experimental_witness_endpoint,
        };

        if self.local_enclave_count == 0 {
            return Err(eyre!("--local-enclave-count must be at least 1"));
        }
        if self.local_enclave_count > 1 && self.server.tee_prover_registry_address.is_none() {
            warn!(
                count = self.local_enclave_count,
                "multiple local enclaves without registry; defaulting to index 0 for routing"
            );
        }
        let transports: Vec<Arc<NitroTransport>> = (0..self.local_enclave_count)
            .map(|_| {
                let server = Arc::new(EnclaveServer::new_local()?);
                Ok(Arc::new(NitroTransport::local(server)))
            })
            .collect::<eyre::Result<Vec<_>>>()?;
        let timeout = Duration::from_secs(self.server.proof_request_timeout_secs);
        let mut server = NitroProverServer::new_multi(prover_config, transports, timeout);
        if let Some(reg) = registration_health {
            server = server.with_registration_health(reg);
        }

        info!(addr = %self.server.listen_addr, "starting nitro prover server (local mode)");
        let handle = server.run(self.server.listen_addr).await?;
        handle.stopped().await;
        Ok(())
    }
}
