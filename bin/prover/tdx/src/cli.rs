//! CLI definition for the Intel TDX TEE prover binary.

use std::{net::SocketAddr, sync::Arc};

use alloy_primitives::Address;
#[cfg(feature = "local")]
use alloy_primitives::B256;
use base_cli_utils::{LogConfig, RuntimeManager};
use base_common_chains::rollup_config;
use base_proof_host::{ProverConfig, ProverService};
use base_proof_tee_tdx_prover::{ProofGenerator, TdxBackend, TdxProverServer};
#[cfg(feature = "local")]
use base_proof_tee_tdx_runtime::StaticTokenProvider;
use base_proof_tee_tdx_runtime::{
    CONFIDENTIAL_SPACE_AUDIENCE, ConfidentialSpaceTokenProvider, TdxAttestationContext, TdxRuntime,
};
use base_proof_worker::{JobDiscovery, JobDiscoveryConfig, ProofSubmitter, WorkerHeartbeatConfig};
use base_prover_service_client::{ProverServiceClientConfig, ProverWorkerClient};
use base_prover_service_protocol::TeeKind;
use clap::Parser;
use eyre::eyre;
use tokio_util::sync::CancellationToken;
use tracing::info;
use uuid::Uuid;

base_cli_utils::define_log_args!("BASE_PROVER_TDX");
base_cli_utils::define_metrics_args!("BASE_PROVER_TDX", 7310);

/// Intel TDX TEE prover binary.
#[derive(Parser)]
#[command(author, version)]
pub(crate) struct Cli {
    /// Run with a deterministic local Confidential Space token fixture.
    #[cfg(feature = "local")]
    #[arg(value_name = "MODE", hide = true)]
    mode: Option<LocalMode>,

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

    /// Socket address for the registrar-facing signer JSON-RPC API.
    #[arg(long, env = "LISTEN_ADDR", default_value = "0.0.0.0:8000")]
    listen_addr: SocketAddr,

    /// Enable experimental `debug_executePayload` witness endpoint.
    #[arg(long, env = "ENABLE_EXPERIMENTAL_WITNESS_ENDPOINT")]
    enable_experimental_witness_endpoint: bool,

    /// Prover-service JSON-RPC endpoint.
    #[arg(long, env = "PROVER_SERVICE_ENDPOINT")]
    prover_service_endpoint: String,

    /// OCI image digest used only by deterministic local development tokens.
    #[cfg(feature = "local")]
    #[arg(long, env = "TEE_TDX_IMAGE_HASH")]
    tee_tdx_image_hash: Option<B256>,

    /// L1 chain ID used when registering this signer.
    #[arg(long, env = "L1_CHAIN_ID")]
    l1_chain_id: Option<u64>,

    /// `TEEProverRegistry` receiving this signer's registration.
    #[arg(long, env = "TEE_PROVER_REGISTRY_ADDRESS")]
    tee_prover_registry_address: Option<Address>,

    /// Logging arguments.
    #[command(flatten)]
    logging: LogArgs,

    /// Metrics arguments.
    #[command(flatten)]
    metrics: MetricsArgs,
}

#[cfg(feature = "local")]
#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
enum LocalMode {
    Local,
}

impl Cli {
    /// Run the TDX prover worker.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let logging = self.logging.clone();
        let metrics = self.metrics.clone();

        LogConfig::from(logging).init_tracing_subscriber()?;
        base_cli_utils::MetricsConfig::from(metrics).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;
        RuntimeManager::new()
            .with_thread_stack_size(8 * 1024 * 1024)
            .run_until_shutdown(|cancel| async move { self.run_worker(cancel).await })
    }

    async fn run_worker(self, cancel: CancellationToken) -> eyre::Result<()> {
        let Self {
            l1_eth_url,
            l2_eth_url,
            l1_beacon_url,
            l2_chain_id,
            listen_addr,
            enable_experimental_witness_endpoint,
            prover_service_endpoint,
            #[cfg(feature = "local")]
            mode,
            #[cfg(feature = "local")]
            tee_tdx_image_hash,
            l1_chain_id,
            tee_prover_registry_address,
            ..
        } = self;
        let rollup_config = rollup_config!(l2_chain_id)
            .ok_or_else(|| eyre!("unknown L2 chain ID: {l2_chain_id}"))?;
        let l1_config = base_common_chains::L1_CONFIGS
            .get(&rollup_config.l1_chain_id)
            .ok_or_else(|| eyre!("unknown L1 chain ID: {}", rollup_config.l1_chain_id))?
            .clone();
        let config = ProverConfig {
            l1_eth_url,
            l2_eth_url,
            l1_beacon_url,
            l2_chain_id,
            rollup_config,
            l1_config,
            enable_experimental_witness_endpoint,
        };
        info!(
            addr = %listen_addr,
            attestation_audience = CONFIDENTIAL_SPACE_AUDIENCE,
            "starting tdx prover worker"
        );
        #[cfg(feature = "local")]
        let runtime = if mode == Some(LocalMode::Local) {
            let image_hash = tee_tdx_image_hash
                .ok_or_else(|| eyre!("TEE_TDX_IMAGE_HASH is required for local TDX prover mode"))?;
            info!(image_hash = %image_hash, "using deterministic local Confidential Space token");
            Arc::new(TdxRuntime::new(
                StaticTokenProvider::for_image_hash(image_hash),
                CONFIDENTIAL_SPACE_AUDIENCE,
            ))
        } else {
            Arc::new(TdxRuntime::new(
                ConfidentialSpaceTokenProvider::new(),
                CONFIDENTIAL_SPACE_AUDIENCE,
            ))
        };
        #[cfg(not(feature = "local"))]
        let runtime = Arc::new(TdxRuntime::new(
            ConfidentialSpaceTokenProvider::new(),
            CONFIDENTIAL_SPACE_AUDIENCE,
        ));
        let registrar_handle = TdxProverServer::new(
            Arc::clone(&runtime),
            l1_chain_id.zip(tee_prover_registry_address).map(|(chain_id, registry_address)| {
                TdxAttestationContext { chain_id, registry_address }
            }),
        )
        .run(listen_addr)
        .await?;
        let prover = ProverService::new(config, TdxBackend::new(runtime));

        let prover_service = ProverServiceClientConfig::new(prover_service_endpoint.clone());
        let client = ProverWorkerClient::connect(&prover_service)?;
        let submitter = ProofSubmitter::new(client.clone());
        let proof_generator =
            Arc::new(ProofGenerator::new(prover, submitter, WorkerHeartbeatConfig::default()));
        let worker_id = format!("tdx-prover-{}", Uuid::new_v4());
        let discovery_config = JobDiscoveryConfig::tee(worker_id.clone(), [TeeKind::IntelTdx]);
        let discovery = JobDiscovery::new(client, proof_generator, discovery_config);

        info!(
            worker_id = %worker_id,
            prover_service_endpoint = %prover_service_endpoint,
            "tdx prover worker started"
        );
        discovery.run_until_cancelled(cancel).await;
        let _ = registrar_handle.stop();
        registrar_handle.stopped().await;
        Ok(())
    }
}
