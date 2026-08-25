//! In-process ZK host that claims jobs from [`crate::InProcessProverService`].

use std::{fs, time::Duration};

use async_trait::async_trait;
use base_proof_zk_backend::SuccinctZkProversConfig;
use base_proof_zk_host::{ProofGeneratorHeartbeatConfig, ZkHost, ZkHostConfig};
use base_prover_service_client::{
    ProverServiceClientConfig, ProverServiceClientError, ProverWorkerClient, ProverWorkerProvider,
};
use base_prover_service_protocol::{
    GetNextProofRequest, GetNextProofResponse, GetProofSessionRequest, GetProofSessionResponse,
    HeartbeatRequest, HeartbeatResponse, RecordProofSessionRequest, RecordProofSessionResponse,
    WorkerSubmitProofRequest, WorkerSubmitProofResponse, ZkBackend,
};
use eyre::{OptionExt, Result, WrapErr, bail, ensure};
use nanoid::nanoid;
use tempfile::TempDir;
use tokio::{sync::watch, task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;
use tracing::info;
use url::Url;

use crate::SystemTestStack;

/// Short discovery poll so tests do not wait on the production 5s interval.
const DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Dry-run SP1 execute can outlive the production 300s default lock.
const LOCK_DURATION_SECONDS: u32 = 1800;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const MAX_CONSECUTIVE_HEARTBEAT_FAILURES: u32 = 5;
const DEFAULT_SEQUENCE_WINDOW: u64 = 50;
const RANGE_CYCLE_LIMIT: u64 = 1_000_000_000_000;
const RANGE_GAS_LIMIT: u64 = 1_000_000_000_000;
/// Bound for the first worker `get_next_proof` after spawn.
const FIRST_WORKER_POLL_TIMEOUT: Duration = Duration::from_secs(10);

/// In-process SP1 ZK host bound to a live [`SystemTestStack`].
pub struct InProcessZkHost {
    cancel: CancellationToken,
    join: Option<JoinHandle<()>>,
    /// Keeps OP Succinct L1/L2 config files on disk for the host lifetime.
    _config_dir: TempDir,
}

impl std::fmt::Debug for InProcessZkHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessZkHost").finish_non_exhaustive()
    }
}

impl InProcessZkHost {
    /// Starts a dry-run ZK host that claims from `prover_service_url`.
    ///
    /// Returns after the host has completed one successful worker poll so the
    /// worker namespace and host wiring are exercised. Cluster and network
    /// backends are not supported here.
    pub async fn start(stack: &SystemTestStack, prover_service_url: &Url) -> Result<Self> {
        let config_dir = Self::install_succinct_chain_configs(stack)?;
        let cancel = CancellationToken::new();
        let l2_rpc = stack.l2_rpc_url().wrap_err("failed to read L2 builder RPC URL")?;
        let urls = stack.urls().await.wrap_err("failed to read system test RPC URLs")?;
        let l1_rpc = stack.l1_rpc_url().await.wrap_err("failed to read L1 RPC URL")?;
        let l1_beacon = Url::parse(
            &stack.l1_stack().beacon_url().await.wrap_err("failed to read L1 beacon URL")?,
        )
        .wrap_err("failed to parse L1 beacon URL")?;
        let base_consensus_rpc = Url::parse(&urls.l2_builder_consensus_rpc)
            .wrap_err("failed to parse L2 builder consensus RPC URL")?;

        let config = SuccinctZkProversConfig {
            base_consensus_rpc: Some(base_consensus_rpc),
            l1_rpc: Some(l1_rpc),
            l1_beacon_rpc: Some(l1_beacon),
            l2_rpc: Some(l2_rpc),
            default_sequence_window: DEFAULT_SEQUENCE_WINDOW,
            cluster_rpc: None,
            cluster_timeout_hours: 24,
            s3_bucket: None,
            s3_region: None,
            network_private_key: None,
            use_kms_requester: false,
            network_timeout_hours: 24,
            range_cycle_limit: RANGE_CYCLE_LIMIT,
            range_gas_limit: RANGE_GAS_LIMIT,
            aggregation_cycle_limit: RANGE_CYCLE_LIMIT,
            aggregation_gas_limit: RANGE_GAS_LIMIT,
            l1_config_dir: Some(config_dir.path().join("L1")),
            l2_config_dir: Some(config_dir.path().join("L2")),
        };
        let Some(provers) = config
            .build_until_cancelled(&cancel)
            .await
            .wrap_err("failed to initialize dry-run ZK backend")?
        else {
            bail!("dry-run ZK backend initialization was cancelled");
        };
        ensure!(
            provers.contains_key(&ZkBackend::DryRun),
            "dry-run ZK backend was not enabled after initialization"
        );

        let client_config = ProverServiceClientConfig::new(prover_service_url.as_str())
            .with_request_timeout(Duration::from_secs(30));
        let inner = ProverWorkerClient::connect(&client_config)
            .wrap_err("failed to connect ZK host to in-process prover-service")?;
        let (first_poll, mut first_poll_rx) = watch::channel(false);
        let client = FirstPollWorker { inner, first_poll };

        let worker_id = format!("system-test-zk-host-{}", nanoid!());
        let host_config = ZkHostConfig::sp1(worker_id.clone())
            .with_job_discovery_poll_interval(DISCOVERY_POLL_INTERVAL)
            .with_job_discovery_lock_duration_seconds(LOCK_DURATION_SECONDS)
            .with_proof_generator_heartbeat(
                ProofGeneratorHeartbeatConfig::with_max_consecutive_failures(
                    HEARTBEAT_INTERVAL,
                    LOCK_DURATION_SECONDS,
                    MAX_CONSECUTIVE_HEARTBEAT_FAILURES,
                ),
            );
        let host = ZkHost::new(client, provers, host_config);
        let run_cancel = cancel.clone();
        let join = tokio::spawn(async move {
            host.run_until_cancelled(run_cancel).await;
        });
        let first_poll =
            timeout(FIRST_WORKER_POLL_TIMEOUT, first_poll_rx.wait_for(|ready| *ready)).await;
        if !matches!(first_poll, Ok(Ok(_))) {
            cancel.cancel();
            join.abort();
        }
        first_poll
            .wrap_err("in-process ZK host did not complete a worker poll")?
            .wrap_err("in-process ZK host stopped before the first worker poll")?;
        info!(worker_id = %worker_id, "started in-process ZK host");

        Ok(Self { cancel, join: Some(join), _config_dir: config_dir })
    }

    /// Writes the stack's L1 chain config into a temp dir for the OP Succinct fetcher.
    ///
    /// System-test L1 is chain 1337, which is not in the built-in `L1_CONFIGS` map. The
    /// fetcher reads `<l1_config_dir>/<chain_id>.json` before that map. L2 configs are
    /// written to a sibling directory so rollup config is not written into the process cwd.
    fn install_succinct_chain_configs(stack: &SystemTestStack) -> Result<TempDir> {
        let genesis: serde_json::Value = serde_json::from_str(
            &stack.l1_genesis().read_el_genesis().wrap_err("failed to read L1 genesis")?,
        )
        .wrap_err("failed to parse L1 genesis")?;
        let config = genesis.get("config").ok_or_eyre("L1 genesis is missing config")?;
        let chain_id = config
            .get("chainId")
            .and_then(serde_json::Value::as_u64)
            .ok_or_eyre("L1 genesis config is missing chainId")?;

        let dir = tempfile::tempdir().wrap_err("failed to create succinct config directory")?;
        let l1_dir = dir.path().join("L1");
        fs::create_dir_all(&l1_dir).wrap_err("failed to create succinct L1 config directory")?;
        fs::write(
            l1_dir.join(format!("{chain_id}.json")),
            serde_json::to_vec_pretty(config).wrap_err("failed to encode L1 chain config")?,
        )
        .wrap_err("failed to write L1 chain config for succinct fetcher")?;

        info!(chain_id, "installed succinct L1/L2 config directories");
        Ok(dir)
    }
}

impl Drop for InProcessZkHost {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

/// Forwards worker RPCs and records the first successful [`get_next_proof`](ProverWorkerProvider::get_next_proof).
#[derive(Clone, Debug)]
struct FirstPollWorker {
    inner: ProverWorkerClient,
    first_poll: watch::Sender<bool>,
}

#[async_trait]
impl ProverWorkerProvider for FirstPollWorker {
    async fn get_next_proof(
        &self,
        request: GetNextProofRequest,
    ) -> Result<GetNextProofResponse, ProverServiceClientError> {
        let response = self.inner.get_next_proof(request).await?;
        let _ = self.first_poll.send(true);
        Ok(response)
    }

    async fn heartbeat(
        &self,
        request: HeartbeatRequest,
    ) -> Result<HeartbeatResponse, ProverServiceClientError> {
        self.inner.heartbeat(request).await
    }

    async fn submit_proof(
        &self,
        request: WorkerSubmitProofRequest,
    ) -> Result<WorkerSubmitProofResponse, ProverServiceClientError> {
        self.inner.submit_proof(request).await
    }

    async fn get_proof_session(
        &self,
        request: GetProofSessionRequest,
    ) -> Result<GetProofSessionResponse, ProverServiceClientError> {
        self.inner.get_proof_session(request).await
    }

    async fn record_proof_session(
        &self,
        request: RecordProofSessionRequest,
    ) -> Result<RecordProofSessionResponse, ProverServiceClientError> {
        self.inner.record_proof_session(request).await
    }
}
