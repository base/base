#![doc = include_str!("../README.md")]

mod backends;
pub use backends::{
    ArtifactClientWrapper, ArtifactStorageConfig, BackendConfig, BackendRegistry, BackendType,
    L1HeadCalculator, OP_SUCCINCT_DRY_RUN_METADATA_KEY, OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY,
    OpSuccinctClusterBackend, OpSuccinctDryRunBackend, OpSuccinctMockBackend,
    OpSuccinctNetworkBackend, OpSuccinctProvider, OpSuccinctStoredExecutionStats,
    OpSuccinctWitnessParams, ProofProcessingResult, ProveResult, ProvingBackend, SessionStatus,
};

mod metrics;
pub use metrics::{
    PROOF_REQUEST_DURATION_MS, PROOF_REQUESTS_COMPLETED, ProverMetrics, REQUESTS,
    RESPONSE_LATENCY_MS, RETRIED_REQUESTS, STUCK_REQUESTS, WITNESS_GENERATION_DURATION_MS,
    WORKER_JOBS_FAILED, api_proof_type_label, inc_proof_requests_completed, inc_requests,
    inc_retried_requests, inc_stuck_requests, inc_worker_jobs_failed, proof_type_label,
    record_proof_request_duration, record_response_latency, record_witness_generation_duration,
};

mod proof_request_manager;
pub use proof_request_manager::ProofRequestManager;

mod proxy;
pub use proxy::{ProxyConfig, ProxyConfigs, RateLimitConfig, start_all_proxies};

mod request;
pub use request::{ExecutionStats, ProveBlockRequest};

#[cfg(feature = "rpc-server")]
mod server;
#[cfg(feature = "rpc-server")]
pub use server::{ProverServiceServer, ServerConfig, WorkerApiConfig};

#[cfg(feature = "rpc-client")]
mod snark_e2e;
#[cfg(feature = "rpc-client")]
pub use snark_e2e::SnarkE2e;

mod worker;
pub use worker::{StatusPoller, WorkerQueueConfig};
