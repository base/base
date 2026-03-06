use std::{
    sync::Arc,
    time::{Instant, SystemTime},
};

use anyhow::{Context, Result};
use op_succinct_elfs::AGGREGATION_ELF;
use op_succinct_host_utils::fetcher::OPSuccinctDataFetcher;
use serde::{Deserialize, Serialize};
use sp1_cluster_artifact::{
    redis::RedisArtifactClient,
    s3::{S3ArtifactClient, S3DownloadMode},
};
use sp1_cluster_common::client::ClusterServiceClient;
use sp1_cluster_utils::{
    check_proof_status, create_request, request_config_from_env, request_proof_from_env,
    ArtifactStoreConfig, ClusterElf, ProofRequest, ProofRequestConfig, ProofRequestResults,
};
use sp1_prover_types::Artifact;
use sp1_sdk::{
    blocking::{CpuProver, Prover as BlockingProver},
    network::proto::types::ProofMode,
    Elf, ProvingKey, SP1ProofMode, SP1ProofWithPublicValues, SP1ProvingKey, SP1Stdin,
    SP1VerifyingKey,
};

/// Get the range ELF depending on the feature flag.
pub fn get_range_elf_embedded() -> &'static [u8] {
    cfg_if::cfg_if! {
        if #[cfg(feature = "celestia")] {
            use op_succinct_elfs::CELESTIA_RANGE_ELF_EMBEDDED;

            CELESTIA_RANGE_ELF_EMBEDDED
        } else if #[cfg(feature = "eigenda")] {
            use op_succinct_elfs::EIGENDA_RANGE_ELF_EMBEDDED;

            EIGENDA_RANGE_ELF_EMBEDDED
        } else {
            use op_succinct_elfs::RANGE_ELF_EMBEDDED;

            RANGE_ELF_EMBEDDED
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(feature = "celestia")] {
        use op_succinct_celestia_host_utils::host::CelestiaOPSuccinctHost;

        /// Initialize the Celestia host.
        pub fn initialize_host(
            fetcher: Arc<OPSuccinctDataFetcher>,
        ) -> Arc<CelestiaOPSuccinctHost> {
            tracing::info!("Initializing host with Celestia DA");
            Arc::new(CelestiaOPSuccinctHost::new(fetcher))
        }
    } else if #[cfg(feature = "eigenda")] {
        use op_succinct_eigenda_host_utils::host::EigenDAOPSuccinctHost;

        /// Initialize the EigenDA host.
        pub fn initialize_host(
            fetcher: Arc<OPSuccinctDataFetcher>,
        ) -> Arc<EigenDAOPSuccinctHost> {
            tracing::info!("Initializing host with EigenDA");
            Arc::new(EigenDAOPSuccinctHost::new(fetcher))
        }
    } else {
        use op_succinct_ethereum_host_utils::host::SingleChainOPSuccinctHost;

        /// Initialize the default (ETH-DA) host.
        pub fn initialize_host(
            fetcher: Arc<OPSuccinctDataFetcher>,
        ) -> Arc<SingleChainOPSuccinctHost> {
            tracing::info!("Initializing host with Ethereum DA");
            Arc::new(SingleChainOPSuccinctHost::new(fetcher))
        }
    }
}

/// Returns true if `SP1_PROVER` is set to `"cluster"` (self-hosted cluster mode).
pub fn is_cluster_mode() -> bool {
    std::env::var("SP1_PROVER").unwrap_or_default() == "cluster"
}

/// Set up range and aggregation proving/verifying keys via blocking CpuProver.
///
/// Runs in `spawn_blocking` because `CpuProver` creates its own tokio runtime
/// internally, which would panic if called directly from an async context.
pub async fn cluster_setup_keys(
) -> Result<(SP1ProvingKey, SP1VerifyingKey, SP1ProvingKey, SP1VerifyingKey)> {
    tokio::task::spawn_blocking(|| {
        let cpu_prover = CpuProver::new();
        let range_pk = cpu_prover
            .setup(Elf::Static(get_range_elf_embedded()))
            .context("range ELF setup failed")?;
        let range_vk = range_pk.verifying_key().clone();
        let agg_pk =
            cpu_prover.setup(Elf::Static(AGGREGATION_ELF)).context("agg ELF setup failed")?;
        let agg_vk = agg_pk.verifying_key().clone();
        anyhow::Ok((range_pk, range_vk, agg_pk, agg_vk))
    })
    .await?
}

fn to_proto_proof_mode(mode: SP1ProofMode) -> ProofMode {
    match mode {
        SP1ProofMode::Core => ProofMode::Core,
        SP1ProofMode::Compressed => ProofMode::Compressed,
        SP1ProofMode::Plonk => ProofMode::Plonk,
        SP1ProofMode::Groth16 => ProofMode::Groth16,
    }
}

/// Generate a proof via a self-hosted SP1 cluster (blocking: waits for completion).
async fn cluster_proof_blocking(
    timeout_secs: u64,
    mode: ProofMode,
    elf: &[u8],
    stdin: SP1Stdin,
    label: &str,
) -> Result<SP1ProofWithPublicValues> {
    tracing::info!("Generating {label} proof via cluster");
    let timeout_hours = timeout_secs.div_ceil(3600).max(1);
    let cluster_elf = ClusterElf::NewElf(elf.to_vec());
    let ProofRequestResults { proof, .. } = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        request_proof_from_env(mode, timeout_hours, cluster_elf, stdin),
    )
    .await
    .map_err(|_| anyhow::anyhow!("cluster {label} proof timed out after {timeout_secs}s"))?
    .map_err(|e| anyhow::anyhow!("cluster {label} proof failed: {e}"))?;
    Ok(SP1ProofWithPublicValues::from(proof))
}

/// Generate a compressed range proof via a self-hosted SP1 cluster.
pub async fn cluster_range_proof(
    timeout_secs: u64,
    stdin: SP1Stdin,
) -> Result<SP1ProofWithPublicValues> {
    cluster_proof_blocking(
        timeout_secs,
        ProofMode::Compressed,
        get_range_elf_embedded(),
        stdin,
        "range",
    )
    .await
}

/// Generate an aggregation proof via a self-hosted SP1 cluster.
pub async fn cluster_agg_proof(
    timeout_secs: u64,
    agg_mode: SP1ProofMode,
    stdin: SP1Stdin,
) -> Result<SP1ProofWithPublicValues> {
    cluster_proof_blocking(
        timeout_secs,
        to_proto_proof_mode(agg_mode),
        AGGREGATION_ELF,
        stdin,
        "aggregation",
    )
    .await
}

// ---------------------------------------------------------------------------
// Async (non-blocking) cluster proving API
// ---------------------------------------------------------------------------

/// Enum dispatching over concrete artifact client types since `ArtifactClient` uses RPITIT
/// (`-> impl Future<...> + Send`) and is NOT object-safe.
/// Matches the pattern in `request_proof_with_config()`.
#[derive(Clone)]
pub enum ClusterArtifactStore {
    Redis(RedisArtifactClient),
    S3(S3ArtifactClient),
}

impl ClusterArtifactStore {
    async fn create_request(
        &self,
        elf: ClusterElf,
        stdin: SP1Stdin,
        config: &ProofRequestConfig,
    ) -> Result<ProofRequest> {
        match self {
            Self::Redis(c) => create_request(c.clone(), elf, stdin, config).await,
            Self::S3(c) => create_request(c.clone(), elf, stdin, config).await,
        }
        .map_err(|e| anyhow::anyhow!("cluster proof submit failed: {e}"))
    }

    async fn check_proof_status(
        &self,
        proof_request: ProofRequest,
        service_client: &ClusterServiceClient,
    ) -> Result<Option<ProofRequestResults>> {
        match self {
            Self::Redis(c) => check_proof_status(c.clone(), proof_request, service_client).await,
            Self::S3(c) => check_proof_status(c.clone(), proof_request, service_client).await,
        }
        .map_err(|e| anyhow::anyhow!("cluster proof poll failed: {e}"))
    }
}

/// Shared cluster configuration constructed once at startup.
/// Always stored behind `Arc` — no `Clone` needed (and `ArtifactStoreConfig` is not `Clone`).
pub struct ClusterProofConfig {
    pub cluster_rpc: String,
    pub artifact_store: ClusterArtifactStore,
    /// The raw `ArtifactStoreConfig` from `request_config_from_env()`, used to construct
    /// per-call `ProofRequestConfig`. We pass the real value rather than a dummy to avoid
    /// breakage if upstream `create_request` ever starts reading this field.
    pub artifact_store_config: ArtifactStoreConfig,
    /// Cached gRPC client for polling only. `create_request()` constructs its own internally.
    pub service_client: ClusterServiceClient,
}

/// In-memory handle for a cluster proof in progress.
pub struct ClusterProofHandle {
    pub proof_request: ProofRequest,
    pub consecutive_poll_failures: u32,
}

/// JSON representation stored in the `cluster_proof_handle` JSONB column.
#[derive(Serialize, Deserialize)]
pub struct ClusterProofHandleJson {
    pub proof_id: String,
    pub proof_output_id: String,
}

impl ClusterProofConfig {
    /// Construct cluster config from environment variables by calling `request_config_from_env()`
    /// once and decomposing the result. The `mode` and `timeout_hours` are per-call parameters
    /// and are ignored here (we pass dummy values).
    pub async fn from_env() -> Result<Self> {
        // Use dummy mode/timeout — these are per-call and only matter for `create_request`.
        let config = request_config_from_env(ProofMode::Compressed, 1);
        let cluster_rpc = config.cluster_rpc.clone();

        let artifact_store = match &config.artifact_store {
            ArtifactStoreConfig::Redis { nodes } => {
                tracing::info!("Cluster using Redis artifact store");
                ClusterArtifactStore::Redis(RedisArtifactClient::new(nodes.clone(), 16))
            }
            ArtifactStoreConfig::S3 { bucket, region } => {
                tracing::info!("Cluster using S3 artifact store");
                let s3_client = S3ArtifactClient::new(
                    region.clone(),
                    bucket.clone(),
                    32,
                    S3DownloadMode::AwsSDK(
                        S3ArtifactClient::create_s3_sdk_download_client(region.clone()).await,
                    ),
                )
                .await;
                ClusterArtifactStore::S3(s3_client)
            }
        };

        let service_client = ClusterServiceClient::new(cluster_rpc.clone())
            .await
            .map_err(|e| anyhow::anyhow!("failed to create cluster service client: {e}"))?;

        Ok(Self {
            cluster_rpc,
            artifact_store,
            artifact_store_config: config.artifact_store,
            service_client,
        })
    }

    /// Build a per-call `ProofRequestConfig` with the specified mode and timeout.
    fn build_request_config(&self, mode: ProofMode, timeout_secs: u64) -> ProofRequestConfig {
        let timeout_hours = timeout_secs.div_ceil(3600).max(1);
        ProofRequestConfig {
            cluster_rpc: self.cluster_rpc.clone(),
            mode,
            timeout_hours,
            artifact_store: match &self.artifact_store_config {
                ArtifactStoreConfig::Redis { nodes } => {
                    ArtifactStoreConfig::Redis { nodes: nodes.clone() }
                }
                ArtifactStoreConfig::S3 { bucket, region } => {
                    ArtifactStoreConfig::S3 { bucket: bucket.clone(), region: region.clone() }
                }
            },
        }
    }
}

/// Submit a range proof to the cluster. Returns immediately with a `ProofRequest` handle.
pub async fn cluster_submit_range_proof(
    config: &ClusterProofConfig,
    timeout_secs: u64,
    stdin: SP1Stdin,
) -> Result<ProofRequest> {
    tracing::info!("Submitting range proof to cluster");
    let req_config = config.build_request_config(ProofMode::Compressed, timeout_secs);
    let cluster_elf = ClusterElf::NewElf(get_range_elf_embedded().to_vec());
    config.artifact_store.create_request(cluster_elf, stdin, &req_config).await
}

/// Submit an aggregation proof to the cluster. Returns immediately with a `ProofRequest` handle.
pub async fn cluster_submit_agg_proof(
    config: &ClusterProofConfig,
    timeout_secs: u64,
    agg_mode: SP1ProofMode,
    stdin: SP1Stdin,
) -> Result<ProofRequest> {
    tracing::info!("Submitting aggregation proof to cluster");
    let proto_mode = to_proto_proof_mode(agg_mode);
    let req_config = config.build_request_config(proto_mode, timeout_secs);
    let cluster_elf = ClusterElf::NewElf(AGGREGATION_ELF.to_vec());
    config.artifact_store.create_request(cluster_elf, stdin, &req_config).await
}

/// Poll the status of a cluster proof. Returns `Ok(Some(results))` if complete,
/// `Ok(None)` if still pending, or `Err` for failures (deadline exceeded, cancelled, etc.).
pub async fn cluster_poll_proof(
    config: &ClusterProofConfig,
    proof_request: ProofRequest,
) -> Result<Option<ProofRequestResults>> {
    config.artifact_store.check_proof_status(proof_request, &config.service_client).await
}

/// Reconstruct a `ProofRequest` from DB-persisted JSON data and timing information.
/// Used to recover in-flight cluster proofs after proposer restart.
pub fn reconstruct_proof_request(
    handle_json: &ClusterProofHandleJson,
    remaining_timeout: std::time::Duration,
) -> ProofRequest {
    ProofRequest {
        proof_id: handle_json.proof_id.clone(),
        proof_output_id: Artifact::from(handle_json.proof_output_id.clone()),
        // Fresh Instant — only used for elapsed-time logging in check_proof_status,
        // so slightly inaccurate elapsed times after restart are harmless.
        start_time: Instant::now(),
        deadline: SystemTime::now() + remaining_timeout,
    }
}
