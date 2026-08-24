//! In-process ZK host that claims jobs from [`crate::InProcessProverService`].

use std::{fs, time::Duration};

use base_proof_zk_backend::SuccinctZkProversConfig;
use base_proof_zk_host::{ProofGeneratorHeartbeatConfig, ZkHost, ZkHostConfig};
use base_prover_service_client::{ProverServiceClientConfig, ProverWorkerClient};
use base_prover_service_protocol::ZkBackend;
use eyre::{OptionExt, Result, WrapErr, bail, ensure};
use nanoid::nanoid;
use tempfile::TempDir;
use tokio::task::JoinHandle;
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

/// In-process SP1 ZK host bound to a live [`SystemTestStack`].
pub struct InProcessZkHost {
    cancel: CancellationToken,
    join: Option<JoinHandle<()>>,
    /// Keeps `L1_CONFIG_DIR` / `L2_CONFIG_DIR` on disk for the host lifetime.
    _config_dir: TempDir,
}

impl std::fmt::Debug for InProcessZkHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessZkHost").finish_non_exhaustive()
    }
}

impl InProcessZkHost {
    /// Starts a ZK host that claims from `prover_service_url` using `backend`.
    ///
    /// Only [`ZkBackend::DryRun`] is supported: cluster and network need remote credentials.
    pub async fn start(
        stack: &SystemTestStack,
        prover_service_url: &Url,
        backend: ZkBackend,
    ) -> Result<Self> {
        if backend != ZkBackend::DryRun {
            bail!("InProcessZkHost only supports ZkBackend::DryRun, got {backend}");
        }

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
        let client = ProverWorkerClient::connect(&client_config)
            .wrap_err("failed to connect ZK host to in-process prover-service")?;

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
        info!(worker_id = %worker_id, "started in-process ZK host");

        Ok(Self { cancel, join: Some(join), _config_dir: config_dir })
    }

    /// Writes the stack's L1 chain config for OP Succinct and points the fetcher at it.
    ///
    /// System-test L1 is chain 1337, which is not in the built-in `L1_CONFIGS` map. The
    /// fetcher reads `<L1_CONFIG_DIR>/<chain_id>.json` before that map. `L2_CONFIG_DIR`
    /// is set so rollup config is not written into the process cwd.
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
        let l2_dir = dir.path().join("L2");
        fs::create_dir_all(&l1_dir).wrap_err("failed to create succinct L1 config directory")?;
        fs::create_dir_all(&l2_dir).wrap_err("failed to create succinct L2 config directory")?;
        fs::write(
            l1_dir.join(format!("{chain_id}.json")),
            serde_json::to_vec_pretty(config).wrap_err("failed to encode L1 chain config")?,
        )
        .wrap_err("failed to write L1 chain config for succinct fetcher")?;

        // SAFETY: only one in-process ZK host runs at a time in these tests, and the
        // directories live as long as this host.
        unsafe {
            std::env::set_var("L1_CONFIG_DIR", &l1_dir);
            std::env::set_var("L2_CONFIG_DIR", &l2_dir);
        }
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
