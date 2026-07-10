//! Metrics definitions and convenience helpers for the prover service.
//!
//! Uses the `metrics` crate facade (`counter!`, `histogram!`) so the exporter
//! backend is determined by the binary (e.g. Prometheus, `DogStatsD`).

use base_prover_service_db::{ApiProofType, ProofJob, ProofType};
use metrics::{counter, describe_counter, describe_histogram, histogram};

// ---------------------------------------------------------------------------
// Metric name constants
// ---------------------------------------------------------------------------

/// Unified RPC request counter. Tags: method, success, `status_code`.
/// Worker-scoped RPCs also include `worker_id` and `prover_id`.
pub const REQUESTS: &str = "prover_service.requests";
/// RPC response latency in milliseconds. Tags: method, success
pub const RESPONSE_LATENCY_MS: &str = "prover_service.response_latency_ms";
/// Time spent in witness generation only. Tags: `proof_type`, success
pub const WITNESS_GENERATION_DURATION_MS: &str = "prover_service.witness_generation_duration_ms";
/// End-to-end wall-clock duration from proof request creation to completion.
/// Tags: `proof_type`, status
pub const PROOF_REQUEST_DURATION_MS: &str = "prover_service.proof_request_duration_ms";
/// Terminal proof request outcomes. Tags: `proof_type`, status (succeeded/failed)
pub const PROOF_REQUESTS_COMPLETED: &str = "prover_service.proof_requests_completed";
/// Stuck requests detected and failed. Tags: `proof_type`
pub const STUCK_REQUESTS: &str = "prover_service.stuck_requests";
/// Stuck requests retried (reset to CREATED). Tags: `proof_type`
pub const RETRIED_REQUESTS: &str = "prover_service.retried_requests";
/// Worker jobs terminally failed by a background reaper. Tags: `reason`, `proof_type`
pub const WORKER_JOBS_FAILED: &str = "prover_service.worker_jobs_failed";

/// Terminal success status label.
pub const PROOF_STATUS_SUCCEEDED: &str = "succeeded";
/// Terminal failure status label.
pub const PROOF_STATUS_FAILED: &str = "failed";

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
        describe_counter!(
            WORKER_JOBS_FAILED,
            "Worker jobs terminally failed by a background reaper"
        );
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

/// Record a worker-scoped RPC request metric. Called once per RPC at handler completion.
pub fn inc_worker_requests(method: &str, success: bool, status_code: &str, worker_id: &str) {
    counter!(REQUESTS,
        "method" => method.to_string(),
        "success" => success.to_string(),
        "status_code" => status_code.to_string(),
        "worker_id" => worker_id.to_string(),
        "prover_id" => worker_id.to_string(),
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

/// Increment the worker-jobs-failed counter for a reaper outcome.
pub fn inc_worker_jobs_failed(reason: &str, proof_type: &str) {
    counter!(WORKER_JOBS_FAILED,
        "reason" => reason.to_string(),
        "proof_type" => proof_type.to_string(),
    )
    .increment(1);
}

/// Record terminal outcome and duration metrics for a proof job.
pub fn record_terminal_proof_job(status: &str, job: &ProofJob) {
    let proof_type = api_proof_type_label(job.api_proof_type);
    inc_proof_requests_completed(status, proof_type);

    if let Some(completed_at) = job.completed_at {
        let duration_ms = (completed_at - job.created_at).num_milliseconds().max(0) as f64;
        record_proof_request_duration(proof_type, status, duration_ms);
    } else {
        tracing::warn!(
            proof_request_id = %job.id,
            status = %status,
            "terminal proof job missing completed_at timestamp"
        );
    }
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

/// Map API proof type to a short string for metric tags.
pub const fn api_proof_type_label(proof_type: ApiProofType) -> &'static str {
    match proof_type {
        ApiProofType::Compressed => "compressed",
        ApiProofType::SnarkGroth16 => "snark_groth16",
        ApiProofType::Tee => "tee",
    }
}

#[cfg(test)]
mod tests {
    use base_prover_service_db::{ApiProofType, ProofJobStatus};
    use chrono::{Duration, Utc};
    use metrics_util::{
        MetricKind,
        debugging::{DebugValue, DebuggingRecorder},
    };
    use uuid::Uuid;

    use super::*;

    #[test]
    fn terminal_proof_job_records_outcome_and_duration() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let created_at = Utc::now();
        let completed_at = created_at + Duration::milliseconds(2500);
        let job = proof_job(created_at, Some(completed_at));

        metrics::with_local_recorder(&recorder, || {
            record_terminal_proof_job(PROOF_STATUS_SUCCEEDED, &job);
        });

        let snapshot = snapshotter.snapshot().into_vec();
        assert_eq!(
            snapshot.iter().find_map(|(ck, _, _, value)| {
                if ck.kind() != MetricKind::Counter || ck.key().name() != PROOF_REQUESTS_COMPLETED {
                    return None;
                }
                if !ck
                    .key()
                    .labels()
                    .any(|label| label.key() == "status" && label.value() == PROOF_STATUS_SUCCEEDED)
                {
                    return None;
                }
                if !ck
                    .key()
                    .labels()
                    .any(|label| label.key() == "proof_type" && label.value() == "tee")
                {
                    return None;
                }
                match value {
                    DebugValue::Counter(value) => Some(*value),
                    _ => None,
                }
            }),
            Some(1),
        );
        assert!(
            snapshot.iter().any(|(ck, _, _, value)| {
                ck.kind() == MetricKind::Histogram
                    && ck.key().name() == PROOF_REQUEST_DURATION_MS
                    && ck.key().labels().any(|label| {
                        label.key() == "status" && label.value() == PROOF_STATUS_SUCCEEDED
                    })
                    && ck
                        .key()
                        .labels()
                        .any(|label| label.key() == "proof_type" && label.value() == "tee")
                    && matches!(value, DebugValue::Histogram(_))
            }),
            "terminal completion should record proof duration",
        );
    }

    #[test]
    fn terminal_proof_job_negative_duration_records_zero_duration() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let created_at = Utc::now();
        let completed_at = created_at - Duration::milliseconds(1);
        let job = proof_job(created_at, Some(completed_at));

        metrics::with_local_recorder(&recorder, || {
            record_terminal_proof_job(PROOF_STATUS_SUCCEEDED, &job);
        });

        let snapshot = snapshotter.snapshot().into_vec();
        assert_eq!(
            snapshot.iter().find_map(|(ck, _, _, value)| {
                if ck.kind() != MetricKind::Histogram
                    || ck.key().name() != PROOF_REQUEST_DURATION_MS
                {
                    return None;
                }
                if !ck
                    .key()
                    .labels()
                    .any(|label| label.key() == "status" && label.value() == PROOF_STATUS_SUCCEEDED)
                {
                    return None;
                }
                if !ck
                    .key()
                    .labels()
                    .any(|label| label.key() == "proof_type" && label.value() == "tee")
                {
                    return None;
                }
                match value {
                    DebugValue::Histogram(values) => {
                        Some(values.iter().map(|value| (*value).into_inner()).collect::<Vec<_>>())
                    }
                    _ => None,
                }
            }),
            Some(vec![0.0]),
        );
    }

    #[test]
    fn worker_request_records_worker_and_prover_labels() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            inc_worker_requests("Heartbeat", true, "OK", "worker-1");
        });

        let snapshot = snapshotter.snapshot().into_vec();
        assert_eq!(
            snapshot.iter().find_map(|(ck, _, _, value)| {
                if ck.kind() != MetricKind::Counter || ck.key().name() != REQUESTS {
                    return None;
                }
                if !ck
                    .key()
                    .labels()
                    .any(|label| label.key() == "method" && label.value() == "Heartbeat")
                {
                    return None;
                }
                if !ck
                    .key()
                    .labels()
                    .any(|label| label.key() == "worker_id" && label.value() == "worker-1")
                {
                    return None;
                }
                if !ck
                    .key()
                    .labels()
                    .any(|label| label.key() == "prover_id" && label.value() == "worker-1")
                {
                    return None;
                }
                match value {
                    DebugValue::Counter(value) => Some(*value),
                    _ => None,
                }
            }),
            Some(1),
        );
    }

    fn proof_job(
        created_at: chrono::DateTime<Utc>,
        completed_at: Option<chrono::DateTime<Utc>>,
    ) -> ProofJob {
        ProofJob {
            id: Uuid::new_v4(),
            session_id: "session-1".to_owned(),
            request_payload: serde_json::Value::Null,
            api_proof_type: ApiProofType::Tee,
            zk_vm: None,
            tee_kind: None,
            job_status: ProofJobStatus::Succeeded,
            attempt: 1,
            worker_id: Some("worker-1".to_owned()),
            lock_id: Some(Uuid::new_v4()),
            lock_expires_at: None,
            claimed_at: None,
            last_heartbeat_at: None,
            error_message: None,
            result_payload: None,
            created_at,
            updated_at: completed_at.unwrap_or(created_at),
            completed_at,
        }
    }
}
