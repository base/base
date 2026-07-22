//! CLI definition for the Nitro TEE prover host binary.

#[cfg(any(target_os = "linux", feature = "local"))]
use std::net::SocketAddr;
#[cfg(any(target_os = "linux", feature = "local"))]
use std::sync::Arc;
#[cfg(any(target_os = "linux", feature = "local"))]
use std::time::Duration;
use std::{
    fs::File,
    path::{Path, PathBuf},
};

use alloy_primitives::Address;
use base_cli_utils::{LogConfig, RuntimeManager};
#[cfg(any(target_os = "linux", feature = "local"))]
use base_common_chains::rollup_config;
#[cfg(any(target_os = "linux", feature = "local"))]
use base_proof_host::ProverConfig;
use base_proof_primitives::PerChainConfig;
#[cfg(feature = "local")]
use base_proof_tee_nitro_enclave::Server as EnclaveServer;
#[cfg(target_os = "linux")]
use base_proof_tee_nitro_enclave::VSOCK_PORT;
#[cfg(any(target_os = "linux", feature = "local"))]
use base_proof_tee_nitro_host::{
    DEFAULT_PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS,
    DEFAULT_PROOF_GENERATOR_MAX_CONSECUTIVE_HEARTBEAT_FAILURES, NitroEnclavePool, NitroHost,
    NitroProverServer, NitroTransport, ProofGeneratorHeartbeatConfig, RegistrationChecker,
    RegistrationHealthConfig,
};
#[cfg(any(target_os = "linux", feature = "local"))]
use base_proof_worker::{
    DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS, DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS,
};
#[cfg(any(target_os = "linux", feature = "local"))]
use base_prover_service_client::{ProverServiceClientConfig, ProverWorkerClient};
use clap::{Parser, Subcommand};
use eyre::eyre;
#[cfg(any(target_os = "linux", feature = "local"))]
use tokio_util::sync::CancellationToken;
#[cfg(any(target_os = "linux", feature = "local"))]
use tracing::info;
#[cfg(feature = "local")]
use tracing::warn;
#[cfg(any(target_os = "linux", feature = "local"))]
use uuid::Uuid;

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
    /// Compute the on-chain config hash for a chain and print it as `0x…`.
    ///
    /// Reads the chain's `RollupConfig` from a JSON file (e.g. `rollup.json`) and hashes
    /// it with the same `PerChainConfig` encoding the enclave uses, so the printed value is
    /// exactly what must be set as `AggregateVerifier.CONFIG_HASH`. Computed offline with no
    /// RPC and no running nodes. Available on all targets — no enclave/NSM required.
    ConfigHash(ConfigHashArgs),

    /// Claim Nitro TEE jobs from prover-service and forward them to the enclave over vsock.
    #[cfg(target_os = "linux")]
    Server(ServerArgs),

    /// Claim Nitro TEE jobs from prover-service using in-process local enclave instances.
    #[cfg(feature = "local")]
    Local(LocalArgs),
}

/// Arguments for the `config-hash` subcommand.
#[derive(Parser)]
struct ConfigHashArgs {
    /// Path to the chain's rollup config JSON (e.g. `rollup.json`).
    #[arg(long, env = "ROLLUP_CONFIG")]
    rollup_config: PathBuf,
}

impl ConfigHashArgs {
    fn run(self) -> eyre::Result<()> {
        let rollup_config = load_rollup_config(&self.rollup_config)?;
        let mut per_chain = PerChainConfig::from_rollup_config(&rollup_config)
            .ok_or_else(|| eyre!("rollup config is missing genesis.system_config"))?;
        per_chain.force_defaults();
        // Print only the hash to stdout so callers can capture it directly.
        println!("{:#x}", per_chain.hash());
        Ok(())
    }
}

/// Shared arguments for Nitro proving workers.
#[derive(Parser)]
struct ProverRuntimeArgs {
    /// L1 execution layer RPC URL.
    #[arg(long, env = "L1_ETH_URL")]
    l1_eth_url: String,

    /// L2 execution layer RPC URL.
    #[arg(long, env = "L2_ETH_URL")]
    l2_eth_url: String,

    /// L2 rollup RPC URL.
    #[arg(long, env = "L2_NODE_URL")]
    l2_node_url: String,

    /// L1 beacon API URL.
    #[arg(long, env = "L1_BEACON_URL")]
    l1_beacon_url: Option<String>,

    /// Run proving without an L1 beacon API (calldata-only / appchain mode).
    #[arg(long, env = "L1_CALLDATA_ONLY")]
    l1_calldata_only: bool,

    /// L2 chain ID.
    #[arg(long, env = "L2_CHAIN_ID")]
    l2_chain_id: u64,

    /// Enable experimental `debug_executePayload` witness endpoint.
    #[arg(long, env = "ENABLE_EXPERIMENTAL_WITNESS_ENDPOINT")]
    enable_experimental_witness_endpoint: bool,

    /// `TEEProverRegistry` contract address on L1. When set, proving is guarded
    /// by onchain signer validity and server `/healthz` is registration-gated.
    #[arg(long, env = "TEE_PROVER_REGISTRY_ADDRESS")]
    tee_prover_registry_address: Option<Address>,

    /// Path to the chain's rollup config JSON (e.g. `rollup.json`), read at startup.
    /// Intended for devnet/ephemeral chains whose chain ID is not in the built-in
    /// config table. For built-in chain IDs, the enclave's `BootInfo::load` uses the
    /// hardcoded config regardless.
    #[arg(long, env = "ROLLUP_CONFIG")]
    rollup_config: Option<PathBuf>,
}

#[cfg(any(target_os = "linux", feature = "local"))]
impl ProverRuntimeArgs {
    fn l1_beacon_url(&self) -> eyre::Result<Option<String>> {
        let l1_beacon_url =
            self.l1_beacon_url.as_deref().map(str::trim).filter(|url| !url.is_empty());

        match (l1_beacon_url, self.l1_calldata_only) {
            (Some(url), false) => Ok(Some(url.to_string())),
            (None, true) => Ok(None),
            (None, false) => Err(eyre!(
                "no L1 derivation mode selected: provide --l1-beacon-url <url> for blob-backed \
                 proving, or pass --l1-calldata-only for calldata-only proving"
            )),
            (Some(_), true) => Err(eyre!(
                "conflicting L1 derivation modes: --l1-beacon-url and --l1-calldata-only are \
                 mutually exclusive"
            )),
        }
    }

    fn registration_health_config(&self) -> Option<RegistrationHealthConfig> {
        self.tee_prover_registry_address.map(|address| RegistrationHealthConfig {
            registry_address: address,
            l1_rpc_url: self.l1_eth_url.clone(),
        })
    }

    fn prover_config(self) -> eyre::Result<ProverConfig> {
        let l1_beacon_url = self.l1_beacon_url()?;

        let rollup_config = if let Some(path) = self.rollup_config.as_deref() {
            let cfg = load_rollup_config(path)?;

            if cfg.l2_chain_id.id() != self.l2_chain_id {
                return Err(eyre!(
                    "chain ID mismatch: --l2-chain-id is {} but {} contains {}",
                    self.l2_chain_id,
                    path.display(),
                    cfg.l2_chain_id.id(),
                ));
            }

            let sc = cfg.genesis.system_config.as_ref();
            info!(
                path = %path.display(),
                genesis_l1_hash = %cfg.genesis.l1.hash,
                genesis_l1_number = cfg.genesis.l1.number,
                genesis_l2_hash = %cfg.genesis.l2.hash,
                genesis_l2_number = cfg.genesis.l2.number,
                genesis_l2_time = cfg.genesis.l2_time,
                block_time = cfg.block_time,
                deposit_contract = %cfg.deposit_contract_address,
                system_config_address = %cfg.l1_system_config_address,
                batcher_address = %sc.map(|sc| sc.batcher_address.to_string()).unwrap_or_default(),
                gas_limit = sc.map(|sc| sc.gas_limit).unwrap_or_default(),
                scalar = %sc.map(|sc| sc.scalar).unwrap_or_default(),
                "using rollup config from file"
            );

            cfg
        } else {
            rollup_config!(self.l2_chain_id)
                .ok_or_else(|| eyre!("unknown L2 chain ID: {}", self.l2_chain_id))?
        };

        let l1_config = base_common_chains::L1_CONFIGS
            .get(&rollup_config.l1_chain_id)
            .ok_or_else(|| eyre!("unknown L1 chain ID: {}", rollup_config.l1_chain_id))?
            .clone();

        Ok(ProverConfig {
            l1_eth_url: self.l1_eth_url,
            l2_eth_url: self.l2_eth_url,
            l2_node_url: self.l2_node_url,
            l1_beacon_url,
            l2_chain_id: self.l2_chain_id,
            rollup_config,
            l1_config,
            enable_experimental_witness_endpoint: self.enable_experimental_witness_endpoint,
        })
    }
}

/// Arguments for the worker `server` subcommand.
#[cfg(target_os = "linux")]
#[derive(Parser)]
struct ServerArgs {
    #[command(flatten)]
    runtime: ProverRuntimeArgs,

    #[command(flatten)]
    worker: WorkerArgs,

    /// Socket address for the registrar-facing signer JSON-RPC API.
    #[arg(long, env = "LISTEN_ADDR", default_value = "0.0.0.0:8000")]
    listen_addr: SocketAddr,

    /// Vsock CID(s) of the enclave(s), comma-separated for multi-enclave mode.
    #[arg(long, env = "VSOCK_CID", value_delimiter = ',')]
    vsock_cid: Vec<u32>,
}

/// Arguments for claiming and generating Nitro proof jobs.
#[cfg(any(target_os = "linux", feature = "local"))]
#[derive(Parser)]
struct WorkerArgs {
    /// Prover-service JSON-RPC endpoint.
    #[arg(long, env = "PROVER_SERVICE_ENDPOINT")]
    prover_service_endpoint: String,

    /// Prover-service JSON-RPC request timeout in seconds.
    #[arg(long, env = "PROVER_SERVICE_REQUEST_TIMEOUT_SECS", default_value_t = 60)]
    prover_service_request_timeout_secs: u64,

    /// Delay after an empty or failed discovery attempt, in milliseconds.
    #[arg(long, env = "JOB_DISCOVERY_POLL_INTERVAL_MS", default_value_t = 5_000)]
    job_discovery_poll_interval_ms: u64,

    /// Requested claim lock duration in seconds. Zero uses the server default.
    #[arg(
        long,
        env = "JOB_DISCOVERY_LOCK_DURATION_SECONDS",
        default_value_t = DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS
    )]
    job_discovery_lock_duration_seconds: u32,

    /// Maximum number of claimed proof jobs generated concurrently.
    #[arg(
        long,
        env = "JOB_DISCOVERY_MAX_CONCURRENT_JOBS",
        default_value_t = DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS
    )]
    job_discovery_max_concurrent_jobs: usize,

    /// Delay between worker API heartbeats while an enclave proof is being generated.
    #[arg(long, env = "PROOF_GENERATOR_HEARTBEAT_INTERVAL_SECS", default_value_t = 30)]
    proof_generator_heartbeat_interval_secs: u64,

    /// Requested heartbeat lock duration in seconds. Zero uses the server default.
    #[arg(
        long,
        env = "PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS",
        default_value_t = DEFAULT_PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS
    )]
    proof_generator_heartbeat_lock_duration_seconds: u32,

    /// Maximum consecutive retryable heartbeat failures before aborting generation.
    #[arg(
        long,
        env = "PROOF_GENERATOR_MAX_CONSECUTIVE_HEARTBEAT_FAILURES",
        default_value_t = DEFAULT_PROOF_GENERATOR_MAX_CONSECUTIVE_HEARTBEAT_FAILURES
    )]
    proof_generator_max_consecutive_heartbeat_failures: u32,
}

impl Cli {
    /// Run the selected worker subcommand.
    pub(crate) fn run(self) -> eyre::Result<()> {
        let Self { command, logging, metrics } = self;
        LogConfig::from(logging).init_tracing_subscriber()?;
        base_cli_utils::MetricsConfig::from(metrics).init_with(|| {
            base_cli_utils::register_version_metrics!();
        })?;

        RuntimeManager::new().with_thread_stack_size(8 * 1024 * 1024).run_until_shutdown(
            |cancel| async move {
                #[cfg(not(any(target_os = "linux", feature = "local")))]
                let _ = cancel;

                match command {
                    Command::ConfigHash(args) => args.run(),
                    #[cfg(target_os = "linux")]
                    Command::Server(args) => args.run(cancel).await,
                    #[cfg(feature = "local")]
                    Command::Local(args) => args.run(cancel).await,
                }
            },
        )
    }
}

#[cfg(target_os = "linux")]
impl ServerArgs {
    async fn run(self, cancel: CancellationToken) -> eyre::Result<()> {
        if self.vsock_cid.is_empty() {
            return Err(eyre!("at least one --vsock-cid is required"));
        }
        let transports = vsock_transports(&self.vsock_cid);

        info!(cids = ?self.vsock_cid, "configured vsock CIDs");
        run_worker(
            self.runtime,
            self.worker,
            transports,
            WorkerTransportMode::Vsock,
            Some(self.listen_addr),
            cancel,
        )
        .await
    }
}

/// Arguments for the worker `local` subcommand.
#[cfg(feature = "local")]
#[derive(Parser)]
struct LocalArgs {
    #[command(flatten)]
    runtime: ProverRuntimeArgs,

    #[command(flatten)]
    worker: WorkerArgs,

    /// Number of local enclave instances to run (minimum 1).
    #[arg(long, env = "LOCAL_ENCLAVE_COUNT", default_value = "1")]
    local_enclave_count: usize,
}

#[cfg(feature = "local")]
impl LocalArgs {
    async fn run(self, cancel: CancellationToken) -> eyre::Result<()> {
        if self.local_enclave_count == 0 {
            return Err(eyre!("--local-enclave-count must be at least 1"));
        }

        let transports = local_transports(self.local_enclave_count)?;
        run_worker(self.runtime, self.worker, transports, WorkerTransportMode::Local, None, cancel)
            .await
    }
}

#[cfg(feature = "local")]
fn local_transports(count: usize) -> eyre::Result<Vec<Arc<NitroTransport>>> {
    (0..count)
        .map(|_| {
            let server = Arc::new(EnclaveServer::new_local()?);
            Ok(Arc::new(NitroTransport::local(server)))
        })
        .collect::<eyre::Result<Vec<_>>>()
}

#[cfg(target_os = "linux")]
fn vsock_transports(cids: &[u32]) -> Vec<Arc<NitroTransport>> {
    cids.iter().map(|&cid| Arc::new(NitroTransport::vsock(cid, VSOCK_PORT))).collect()
}

#[cfg(any(target_os = "linux", feature = "local"))]
async fn run_worker(
    runtime: ProverRuntimeArgs,
    worker: WorkerArgs,
    transports: Vec<Arc<NitroTransport>>,
    transport_mode: WorkerTransportMode,
    registrar_listen_addr: Option<SocketAddr>,
    cancel: CancellationToken,
) -> eyre::Result<()> {
    let registration_health = runtime.registration_health_config();
    let registry_configured = registration_health.is_some();
    let config = runtime.prover_config()?;

    if transports.is_empty() {
        return Err(eyre!("at least one enclave transport is required"));
    }
    let enclave_count = transports.len();
    if enclave_count > 1 && !registry_configured {
        match transport_mode {
            #[cfg(target_os = "linux")]
            WorkerTransportMode::Vsock => {
                return Err(eyre!(
                    "multi-CID requires --tee-prover-registry-address for onchain routing"
                ));
            }
            #[cfg(feature = "local")]
            WorkerTransportMode::Local => {
                warn!(
                    count = enclave_count,
                    "multiple local enclaves without registry; all signers are eligible for routing"
                );
            }
        }
    }

    let mut pool = NitroEnclavePool::new_multi(config, transports);
    let registration_checker = if let Some(registration_health) = &registration_health {
        Some(Arc::new(
            RegistrationChecker::from_health_config(pool.transports(), registration_health)
                .map_err(|e| eyre!("registration checker init failed: {e}"))?,
        ))
    } else {
        None
    };
    if let Some(checker) = &registration_checker {
        pool = pool
            .with_registration_checker(Arc::clone(checker))
            .map_err(|e| eyre!("registration checker init failed: {e}"))?;
    }
    let registrar_transports = pool.transports();

    let prover_service = ProverServiceClientConfig::new(worker.prover_service_endpoint.clone())
        .with_request_timeout(Duration::from_secs(worker.prover_service_request_timeout_secs));

    let client = ProverWorkerClient::connect(&prover_service)?;
    let heartbeat = ProofGeneratorHeartbeatConfig::with_max_consecutive_failures(
        Duration::from_secs(worker.proof_generator_heartbeat_interval_secs),
        worker.proof_generator_heartbeat_lock_duration_seconds,
        worker.proof_generator_max_consecutive_heartbeat_failures,
    );
    let worker_id = format!("nitro-host-{}", Uuid::new_v4());
    let host = NitroHost::new(client, Arc::new(pool), worker_id.clone(), heartbeat)
        .with_poll_interval(Duration::from_millis(worker.job_discovery_poll_interval_ms))
        .with_lock_duration_seconds(worker.job_discovery_lock_duration_seconds)
        .with_max_concurrent_jobs(worker.job_discovery_max_concurrent_jobs);
    let registrar_handle = if let Some(addr) = registrar_listen_addr {
        Some(
            NitroProverServer::run_registrar_rpc_server(
                addr,
                registrar_transports,
                registration_checker,
            )
            .await?,
        )
    } else {
        None
    };

    match transport_mode {
        #[cfg(target_os = "linux")]
        WorkerTransportMode::Vsock => {
            info!(
                worker_id = %worker_id,
                prover_service_endpoint = %worker.prover_service_endpoint,
                enclave_count,
                "starting nitro prover host worker"
            );
        }
        #[cfg(feature = "local")]
        WorkerTransportMode::Local => {
            info!(
                worker_id = %worker_id,
                prover_service_endpoint = %worker.prover_service_endpoint,
                enclave_count,
                "starting nitro prover host worker (local mode)"
            );
        }
    }
    host.run_until_cancelled(cancel).await;
    if let Some(handle) = registrar_handle {
        let _ = handle.stop();
        handle.stopped().await;
    }
    Ok(())
}

#[cfg(any(target_os = "linux", feature = "local"))]
#[derive(Clone, Copy)]
enum WorkerTransportMode {
    #[cfg(target_os = "linux")]
    Vsock,
    #[cfg(feature = "local")]
    Local,
}

fn load_rollup_config(path: &Path) -> eyre::Result<base_common_genesis::RollupConfig> {
    let file = File::open(path)
        .map_err(|e| eyre!("failed to open rollup config file {}: {e}", path.display()))?;
    serde_json::from_reader(file)
        .map_err(|e| eyre!("failed to parse rollup config file {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::b256;
    use base_common_chains::rollup_config;

    use super::{PerChainConfig, load_rollup_config};

    /// The config hash computed from a `rollup.json` file must be byte-identical to the value
    /// the enclave derives on the proving path (`server.rs` `config_hash`) for the same chain.
    /// Round-trips a built-in config through JSON to exercise the file loader end to end.
    #[test]
    fn config_hash_from_file_matches_enclave_value() {
        let rollup = rollup_config!(8453).expect("base mainnet rollup config");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rollup.json");
        std::fs::write(&path, serde_json::to_vec(&rollup).expect("serialize rollup config"))
            .expect("write rollup config");

        let loaded = load_rollup_config(&path).expect("load rollup config");
        let mut per_chain =
            PerChainConfig::from_rollup_config(&loaded).expect("per-chain config from rollup");
        per_chain.force_defaults();

        assert_eq!(
            per_chain.hash(),
            b256!("1607709d90d40904f790574404e2ad614eac858f6162faa0ec34c6bf5e5f3c57"),
        );
    }

    #[cfg(any(target_os = "linux", feature = "local"))]
    mod runtime_args {
        use super::super::ProverRuntimeArgs;

        fn args(l1_beacon_url: Option<&str>, l1_calldata_only: bool) -> ProverRuntimeArgs {
            ProverRuntimeArgs {
                l1_eth_url: "http://localhost:8545".to_string(),
                l2_eth_url: "http://localhost:9545".to_string(),
                l2_node_url: "http://localhost:9546".to_string(),
                l1_beacon_url: l1_beacon_url.map(str::to_string),
                l1_calldata_only,
                l2_chain_id: 8453,
                enable_experimental_witness_endpoint: false,
                tee_prover_registry_address: None,
                rollup_config: None,
            }
        }

        #[test]
        fn beacon_mode_is_valid() {
            let l1_beacon_url = args(Some("http://localhost:5052"), false).l1_beacon_url().unwrap();
            assert_eq!(l1_beacon_url, Some("http://localhost:5052".to_string()));
        }

        #[test]
        fn calldata_only_mode_is_valid() {
            let l1_beacon_url = args(None, true).l1_beacon_url().unwrap();
            assert_eq!(l1_beacon_url, None);
        }

        #[test]
        fn neither_mode_is_an_error() {
            assert!(args(None, false).l1_beacon_url().is_err());
        }

        #[test]
        fn both_modes_are_an_error() {
            assert!(args(Some("http://localhost:5052"), true).l1_beacon_url().is_err());
        }

        #[test]
        fn empty_beacon_url_is_treated_as_missing() {
            assert!(args(Some("  "), false).l1_beacon_url().is_err());
        }
    }
}
