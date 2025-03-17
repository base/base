use metrics::{describe_gauge, gauge};
use metrics_exporter_prometheus::PrometheusBuilder;
use metrics_process::Collector;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    thread,
    time::Duration,
};
use strum::{EnumMessage, IntoEnumIterator};
use strum_macros::{Display, EnumIter};
use tracing::warn;

// Define an enum for all gauge metrics
#[derive(Debug, Clone, Copy, Display, EnumIter, EnumMessage)]
pub enum GaugeMetric {
    // Proof status gauges
    #[strum(
        serialize = "succinct_current_unrequested_proofs",
        message = "Number of proofs currently unrequested"
    )]
    CurrentUnrequestedProofs,
    #[strum(
        serialize = "succinct_current_proving_proofs",
        message = "Number of proofs currently being proved"
    )]
    CurrentProvingProofs,
    #[strum(
        serialize = "succinct_current_witnessgen_proofs",
        message = "Number of proofs currently in witness generation"
    )]
    CurrentWitnessgenProofs,
    #[strum(
        serialize = "succinct_current_execute_proofs",
        message = "Number of proofs currently being executed"
    )]
    CurrentExecuteProofs,

    // Proposer gauges
    #[strum(
        serialize = "succinct_highest_proven_contiguous_block",
        message = "Highest proven contiguous block"
    )]
    HighestProvenContiguousBlock,
    #[strum(
        serialize = "succinct_latest_contract_l2_block",
        message = "Latest L2 block number from the contract"
    )]
    LatestContractL2Block,
    #[strum(
        serialize = "succinct_l2_unsafe_head_block",
        message = "L2 unsafe head block number"
    )]
    L2UnsafeHeadBlock,
    #[strum(
        serialize = "succinct_l2_finalized_block",
        message = "L2 finalized block number"
    )]
    L2FinalizedBlock,
    #[strum(
        serialize = "succinct_min_block_to_prove_to_agg",
        message = "Minimum block number required to prove for aggregation"
    )]
    MinBlockToProveToAgg,
    #[strum(
        serialize = "succinct_proof_request_retry_count",
        message = "Number of proof request retries"
    )]
    ProofRequestRetryCount,

    // Error gauges
    #[strum(
        serialize = "succinct_total_error_count",
        message = "Number of total errors"
    )]
    TotalErrorCount,
    #[strum(
        serialize = "succinct_proof_request_timeout_error_count",
        message = "Number of proof request timeout errors"
    )]
    ProofRequestTimeoutErrorCount,
    #[strum(
        serialize = "succinct_retry_error_count",
        message = "Number of retry errors"
    )]
    RetryErrorCount,
    #[strum(
        serialize = "succinct_witnessgen_error_count",
        message = "Number of witness generation errors"
    )]
    WitnessgenErrorCount,
    #[strum(
        serialize = "succinct_execution_error_count",
        message = "Number of execution errors"
    )]
    ExecutionErrorCount,
    #[strum(
        serialize = "succinct_range_proof_request_error_count",
        message = "Number of range proof request errors"
    )]
    RangeProofRequestErrorCount,
    #[strum(
        serialize = "succinct_agg_proof_request_error_count",
        message = "Number of aggregation proof request errors"
    )]
    AggProofRequestErrorCount,
    #[strum(
        serialize = "succinct_relay_agg_proof_error_count",
        message = "Number of relay aggregation proof errors"
    )]
    RelayAggProofErrorCount,
}

impl GaugeMetric {
    // Helper to describe the gauge
    pub fn describe(&self) {
        describe_gauge!(self.to_string(), self.get_message().unwrap());
    }

    // Helper to set the gauge value
    pub fn set(&self, value: f64) {
        gauge!(self.to_string()).set(value);
    }

    // Helper to increment the gauge value
    pub fn increment(&self, value: f64) {
        gauge!(self.to_string()).increment(value);
    }
}

pub fn custom_gauges() {
    // Register all gauges
    for metric in GaugeMetric::iter() {
        metric.describe();
    }
}

pub fn init_gauges() {
    // Initialize all gauges to 0.0
    for metric in GaugeMetric::iter() {
        metric.set(0.0);
    }
}

pub fn init_metrics(port: &u16) {
    custom_gauges();

    let builder = PrometheusBuilder::new().with_http_listener(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        port.to_owned(),
    ));

    if let Err(e) = builder.install() {
        warn!(
            "Failed to start metrics server: {}. Will continue without metrics.",
            e
        );
    }

    // Spawn a thread to collect process metrics
    thread::spawn(move || {
        let collector = Collector::default();
        collector.describe();
        loop {
            // Periodically call `collect()` method to update information.
            collector.collect();
            thread::sleep(Duration::from_millis(750));
        }
    });
}
