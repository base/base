//! Metrics definitions and convenience helpers for the ZK prover service.
//!
//! Uses the `metrics` crate facade (`counter!`, `histogram!`) so the exporter
//! backend is determined by the binary (e.g. Prometheus, `DogStatsD`).

use base_zk_db::ProofType;
use metrics::{counter, describe_counter, describe_histogram, histogram};

// ---------------------------------------------------------------------------
// Metric name constants
// ---------------------------------------------------------------------------

/// Unified RPC request counter. Tags: method, success, `status_code`
pub const REQUESTS: &str = "zk_prover_service.requests";
/// RPC response latency in milliseconds. Tags: method, success
pub const RESPONSE_LATENCY_MS: &str = "zk_prover_service.response_latency_ms";
/// Time spent in witness generation only. Tags: `proof_type`, success
pub const WITNESS_GENERATION_DURATION_MS: &str = "zk_prover_service.witness_generation_duration_ms";
/// End-to-end wall-clock duration from proof request creation to completion.
/// Tags: `proof_type`, status
pub const PROOF_REQUEST_DURATION_MS: &str = "zk_prover_service.proof_request_duration_ms";
/// Terminal proof request outcomes. Tags: `proof_type`, status (succeeded/failed)
pub const PROOF_REQUESTS_COMPLETED: &str = "zk_prover_service.proof_requests_completed";
/// Stuck requests detected and failed. Tags: `proof_type`
pub const STUCK_REQUESTS: &str = "zk_prover_service.stuck_requests";
/// Stuck requests retried (reset to CREATED). Tags: `proof_type`
pub const RETRIED_REQUESTS: &str = "zk_prover_service.retried_requests";
/// Outbox task submission outcomes. Tags: status (submitted/failed), `proof_type`
pub const OUTBOX_TASKS_PROCESSED: &str = "zk_prover_service.outbox_tasks_processed";

// ---------------------------------------------------------------------------
// ProverMetrics — metric descriptions (called once at init)
// ---------------------------------------------------------------------------

/// Registers metric descriptions with the global recorder.
#[derive(Debug)]
pub struct ProverMetrics;

impl ProverMetrics {
    /// Register metric descriptions with the global recorder.
    /// Must be called after the metrics recorder is installed.
    pub fn init() {
        describe_counter!(REQUESTS, "Unified RPC request counter");
        describe_histogram!(RESPONSE_LATENCY_MS, "RPC response latency (ms)");
        describe_histogram!(
            WITNESS_GENERATION_DURATION_MS,
            "Time spent in witness generation only (ms)"
        );
        describe_histogram!(
            PROOF_REQUEST_DURATION_MS,
            "End-to-end wall-clock proof request duration (ms)"
        );
        describe_counter!(PROOF_REQUESTS_COMPLETED, "Terminal proof request outcomes");
        describe_counter!(STUCK_REQUESTS, "Stuck requests detected and failed");
        describe_counter!(RETRIED_REQUESTS, "Stuck requests retried (reset to CREATED)");
        describe_counter!(OUTBOX_TASKS_PROCESSED, "Outbox task submission outcomes");
    }
}

// ---------------------------------------------------------------------------
// Convenience helpers — thin wrappers around `metrics` crate macros
// ---------------------------------------------------------------------------

/// Record a unified RPC request metric. Called once per RPC at handler completion.
pub fn inc_requests(method: &str, success: bool, status_code: &str) {
    counter!(REQUESTS,
        "method" => method.to_string(),
        "success" => success.to_string(),
        "status_code" => status_code.to_string(),
    )
    .increment(1);
}

/// Record RPC response latency in milliseconds.
pub fn record_response_latency(method: &str, success: bool, duration_ms: f64) {
    histogram!(RESPONSE_LATENCY_MS,
        "method" => method.to_string(),
        "success" => success.to_string(),
    )
    .record(duration_ms);
}

/// Record witness generation duration in milliseconds.
pub fn record_witness_generation_duration(proof_type: &str, success: bool, duration_ms: f64) {
    histogram!(WITNESS_GENERATION_DURATION_MS,
        "proof_type" => proof_type.to_string(),
        "success" => success.to_string(),
    )
    .record(duration_ms);
}

/// Record end-to-end proof request duration in milliseconds.
pub fn record_proof_request_duration(proof_type: &str, status: &str, duration_ms: f64) {
    histogram!(PROOF_REQUEST_DURATION_MS,
        "proof_type" => proof_type.to_string(),
        "status" => status.to_string(),
    )
    .record(duration_ms);
}

/// Increment terminal proof request completion counter.
pub fn inc_proof_requests_completed(status: &str, proof_type: &str) {
    counter!(PROOF_REQUESTS_COMPLETED,
        "status" => status.to_string(),
        "proof_type" => proof_type.to_string(),
    )
    .increment(1);
}

/// Increment stuck requests counter.
pub fn inc_stuck_requests(proof_type: &str) {
    counter!(STUCK_REQUESTS, "proof_type" => proof_type.to_string()).increment(1);
}

/// Increment retried requests counter.
pub fn inc_retried_requests(proof_type: &str) {
    counter!(RETRIED_REQUESTS, "proof_type" => proof_type.to_string()).increment(1);
}

/// Increment outbox task processed counter.
pub fn inc_outbox_tasks_processed(status: &str, proof_type: &str) {
    counter!(OUTBOX_TASKS_PROCESSED,
        "status" => status.to_string(),
        "proof_type" => proof_type.to_string(),
    )
    .increment(1);
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Map proof type to a short string for metric tags.
pub const fn proof_type_label(proof_type: ProofType) -> &'static str {
    match proof_type {
        ProofType::OpSuccinctSp1ClusterCompressed => "compressed",
        ProofType::OpSuccinctSp1ClusterSnarkGroth16 => "snark_groth16",
    }
}

/// Map tonic gRPC status code to a short string for metric tags.
pub const fn grpc_status_code_str(code: tonic::Code) -> &'static str {
    match code {
        tonic::Code::Ok => "OK",
        tonic::Code::Cancelled => "CANCELLED",
        tonic::Code::Unknown => "UNKNOWN",
        tonic::Code::InvalidArgument => "INVALID_ARGUMENT",
        tonic::Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
        tonic::Code::NotFound => "NOT_FOUND",
        tonic::Code::AlreadyExists => "ALREADY_EXISTS",
        tonic::Code::PermissionDenied => "PERMISSION_DENIED",
        tonic::Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
        tonic::Code::FailedPrecondition => "FAILED_PRECONDITION",
        tonic::Code::Aborted => "ABORTED",
        tonic::Code::OutOfRange => "OUT_OF_RANGE",
        tonic::Code::Unimplemented => "UNIMPLEMENTED",
        tonic::Code::Internal => "INTERNAL",
        tonic::Code::Unavailable => "UNAVAILABLE",
        tonic::Code::DataLoss => "DATA_LOSS",
        tonic::Code::Unauthenticated => "UNAUTHENTICATED",
    }
}
