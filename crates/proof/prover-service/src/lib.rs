#![doc = include_str!("../README.md")]

#[allow(
    unreachable_pub,
    clippy::clone_on_ref_ptr,
    clippy::derive_partial_eq_without_eq,
    clippy::doc_markdown,
    clippy::missing_const_for_fn
)]
mod proto {
    tonic::include_proto!("prover_service");
}

pub use proto::prover_service_server;

/// Serialized protobuf `FileDescriptorSet` for the prover service.
pub const PROVER_SERVICE_FILE_DESCRIPTOR_SET: &[u8] =
    tonic::include_file_descriptor_set!("prover_service_descriptor");

pub use proto::{
    ClaimProofJobRequest, ClaimProofJobResponse, CompleteProofJobRequest, CompleteProofJobResponse,
    FailProofJobRequest, FailProofJobResponse, GetProofJobRequest, GetProofJobResponse,
    GetProofRequest, GetProofResponse, HeartbeatProofJobRequest, HeartbeatProofJobResponse,
    ListProofsRequest, ListProofsResponse, ProofJob, ProofJobStatus, ProofRequest, ProofResult,
    ProofStatus, ProofSummary, ProofType, SnarkGroth16ProofRequest, SnarkGroth16ProofResult,
    SubmitProofRequest, SubmitProofResponse, TeeKind, TeeProofRequest, TeeProofResult, TeeProposal,
    ZkProofRequest, ZkProofResult, ZkVm, proof_request, proof_result, prover_service_client,
};

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

mod request;
pub use request::{ExecutionStats, ProveBlockRequest};

mod server;
pub use server::ProverServiceServer;

mod snark_e2e;
pub use snark_e2e::SnarkE2e;

mod worker;
pub use worker::{ProverWorker, ProverWorkerPool, StatusPoller};
