//! RPC metrics unique for Base.

use std::time::Instant;

base_metrics::define_metrics! {
    base_rpc.sequencer,
    struct = SequencerMetrics,
    #[describe("How long it takes to forward a transaction to the sequencer")]
    sequencer_forward_latency: histogram,
}

impl SequencerMetrics {
    /// Records the duration it took to forward a transaction.
    #[inline]
    pub fn record_forward_latency(duration: core::time::Duration) {
        Self::sequencer_forward_latency().record(duration.as_secs_f64());
    }
}

base_metrics::define_metrics! {
    base_rpc.eth_api_ext,
    struct = EthApiExtMetrics,
    #[describe("How long it takes to handle a eth_getProof request successfully")]
    get_proof_latency: histogram,
    #[describe("Total number of eth_getProof requests")]
    get_proof_requests: counter,
    #[describe("Total number of successful eth_getProof responses")]
    get_proof_successful_responses: counter,
    #[describe("Total number of failures handling eth_getProof requests")]
    get_proof_failures: counter,
}

/// Types of debug apis
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum DebugApis {
    /// `DebugExecutePayload` Api
    DebugExecutePayload,
    /// `DebugExecutionWitness` Api
    DebugExecutionWitness,
}

impl DebugApis {
    /// Returns the operation as a string for metrics labels.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::DebugExecutePayload => "debug_execute_payload",
            Self::DebugExecutionWitness => "debug_execution_witness",
        }
    }
}

base_metrics::define_metrics! {
    base_rpc.debug_api_ext,
    struct = DebugApiExtRpcMetrics,
    #[describe("End-to-end time to handle this API call")]
    #[label(api)]
    latency: histogram,
    #[describe("Total number of requests for this API")]
    #[label(api)]
    requests: counter,
    #[describe("Total number of successful responses for this API")]
    #[label(api)]
    successful_responses: counter,
    #[describe("Total number of failures for this API")]
    #[label(api)]
    failures: counter,
}

/// Metrics for Debug API extension calls.
#[derive(Debug)]
pub struct DebugApiExtMetrics;

impl DebugApiExtMetrics {
    /// Record a Debug API call async (tracks latency, requests, success, failures).
    pub async fn record_operation_async<F, T, E>(api: DebugApis, f: F) -> Result<T, E>
    where
        F: Future<Output = Result<T, E>>,
    {
        let label = api.as_str();
        let start = Instant::now();
        let result = f.await;
        DebugApiExtRpcMetrics::latency(label).record(start.elapsed().as_secs_f64());
        DebugApiExtRpcMetrics::requests(label).increment(1);

        if result.is_ok() {
            DebugApiExtRpcMetrics::successful_responses(label).increment(1);
        } else {
            DebugApiExtRpcMetrics::failures(label).increment(1);
        }

        result
    }
}
