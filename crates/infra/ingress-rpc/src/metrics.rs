use metrics::{Counter, Histogram};
use metrics_derive::Metrics;
use tokio::time::Duration;

/// Records an RPC latency histogram sample for the given method name.
pub fn record_histogram(rpc_latency: Duration, rpc: String) {
    metrics::histogram!("tips_ingress_rpc_rpc_latency", "rpc" => rpc)
        .record(rpc_latency.as_secs_f64());
}

/// Prometheus metrics for the tips ingress RPC service.
#[derive(Metrics, Clone)]
#[metrics(scope = "tips_ingress_rpc")]
pub struct Metrics {
    /// Number of valid transactions received.
    #[metric(describe = "Number of valid transactions received")]
    pub transactions_received: Counter,

    /// Number of valid bundles parsed.
    #[metric(describe = "Number of valid bundles parsed")]
    pub bundles_parsed: Counter,

    /// Number of bundles successfully simulated.
    #[metric(describe = "Number of bundles simulated")]
    pub successful_simulations: Counter,

    /// Number of bundles that failed simulation.
    #[metric(describe = "Number of bundles that failed simulation")]
    pub failed_simulations: Counter,

    /// Number of bundles sent to Kafka.
    #[metric(describe = "Number of bundles sent to kafka")]
    pub sent_to_kafka: Counter,

    /// Number of transactions sent to the mempool.
    #[metric(describe = "Number of transactions sent to mempool")]
    pub sent_to_mempool: Counter,

    /// Duration of transaction validation.
    #[metric(describe = "Duration of validate_tx")]
    pub validate_tx_duration: Histogram,

    /// Duration of bundle validation.
    #[metric(describe = "Duration of validate_bundle")]
    pub validate_bundle_duration: Histogram,

    /// Duration of bundle metering.
    #[metric(describe = "Duration of meter_bundle")]
    pub meter_bundle_duration: Histogram,

    /// Duration of send raw transaction.
    #[metric(describe = "Duration of send_raw_transaction")]
    pub send_raw_transaction_duration: Histogram,

    /// Total backrun bundles received.
    #[metric(describe = "Total backrun bundles received")]
    pub backrun_bundles_received_total: Counter,

    /// Duration to send backrun bundle to op-rbuilder.
    #[metric(describe = "Duration to send backrun bundle to op-rbuilder")]
    pub backrun_bundles_sent_duration: Histogram,

    /// Total raw transactions forwarded to additional endpoint.
    #[metric(describe = "Total raw transactions forwarded to additional endpoint")]
    pub raw_tx_forwards_total: Counter,

    /// Number of bundles that exceeded the metering time.
    #[metric(describe = "Number of bundles that exceeded the metering time")]
    pub bundles_exceeded_metering_time: Counter,

    /// Number of bundles cancelled via `eth_cancelBundle`.
    #[metric(describe = "Number of bundles cancelled via eth_cancelBundle")]
    pub bundles_cancelled: Counter,
}
