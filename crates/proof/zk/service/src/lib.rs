#![doc = include_str!("../README.md")]
#![recursion_limit = "256"]

mod backends;
pub use backends::{
    ArtifactClientWrapper, ArtifactStorageConfig, BackendConfig, BackendRegistry, BackendType,
    L1HeadCalculator, OpSuccinctClusterBackend, OpSuccinctMockBackend, OpSuccinctNetworkBackend,
    OpSuccinctProvider, OpSuccinctWitnessParams, ProofProcessingResult, ProveResult,
    ProvingBackend, SessionStatus,
};

mod metrics;
pub use metrics::{
    OUTBOX_TASKS_PROCESSED, PROOF_REQUEST_DURATION_MS, PROOF_REQUESTS_COMPLETED, ProverMetrics,
    REQUESTS, RESPONSE_LATENCY_MS, RETRIED_REQUESTS, STUCK_REQUESTS,
    WITNESS_GENERATION_DURATION_MS, grpc_status_code_str, inc_outbox_tasks_processed,
    inc_proof_requests_completed, inc_requests, inc_retried_requests, inc_stuck_requests,
    proof_type_label, record_proof_request_duration, record_response_latency,
    record_witness_generation_duration,
};

mod proof_request_manager;
pub use proof_request_manager::ProofRequestManager;

mod proxy;
pub use proxy::{ProxyConfig, ProxyConfigs, RateLimitConfig, start_all_proxies};

mod server;
pub use server::ProverServiceServer;

mod snark_e2e;
pub use snark_e2e::SnarkE2e;

mod worker;
pub use worker::{ProverWorker, ProverWorkerPool, StatusPoller};
