use metrics::{Counter, Histogram};
use metrics_derive::Metrics;
use tokio::time::Duration;

pub fn record_histogram(rpc_latency: Duration, rpc: String) {
    metrics::histogram!("tips_ingress_rpc_rpc_latency", "rpc" => rpc)
        .record(rpc_latency.as_secs_f64());
}

#[derive(Metrics, Clone)]
#[metrics(scope = "tips_ingress_rpc")]
pub struct Metrics {
    #[metric(describe = "Number of valid transactions received")]
    pub transactions_received: Counter,

    #[metric(describe = "Number of valid bundles parsed")]
    pub bundles_parsed: Counter,

    #[metric(describe = "Number of bundles simulated")]
    pub successful_simulations: Counter,

    #[metric(describe = "Number of bundles simulated")]
    pub failed_simulations: Counter,

    #[metric(describe = "Number of bundles sent to kafka")]
    pub sent_to_kafka: Counter,

    #[metric(describe = "Number of transactions sent to mempool")]
    pub sent_to_mempool: Counter,

    #[metric(describe = "Duration of validate_tx")]
    pub validate_tx_duration: Histogram,

    #[metric(describe = "Duration of validate_bundle")]
    pub validate_bundle_duration: Histogram,

    #[metric(describe = "Duration of meter_bundle")]
    pub meter_bundle_duration: Histogram,

    #[metric(describe = "Duration of send_raw_transaction")]
    pub send_raw_transaction_duration: Histogram,

    #[metric(describe = "Total backrun bundles received")]
    pub backrun_bundles_received_total: Counter,

    #[metric(describe = "Duration to send backrun bundle to op-rbuilder")]
    pub backrun_bundles_sent_duration: Histogram,

    #[metric(describe = "Total raw transactions forwarded to additional endpoint")]
    pub raw_tx_forwards_total: Counter,
}
