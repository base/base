#![doc = include_str!("../README.md")]

mod metrics;
pub use metrics::{
    PROOF_REQUEST_DURATION_MS, PROOF_REQUESTS_COMPLETED, ProverMetrics, REQUESTS,
    RESPONSE_LATENCY_MS, RETRIED_REQUESTS, STUCK_REQUESTS, WITNESS_GENERATION_DURATION_MS,
    WORKER_JOBS_FAILED, api_proof_type_label, inc_proof_requests_completed, inc_requests,
    inc_retried_requests, inc_stuck_requests, inc_worker_jobs_failed, proof_type_label,
    record_proof_request_duration, record_response_latency, record_witness_generation_duration,
};

mod metadata;
pub use metadata::{OP_SUCCINCT_DRY_RUN_METADATA_KEY, OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY};

#[cfg(feature = "rpc-server")]
mod server;
#[cfg(feature = "rpc-server")]
pub use server::{ProverServiceServer, ServerConfig, WorkerApiConfig};

mod worker;
pub use worker::{StatusPoller, WorkerQueueConfig};
