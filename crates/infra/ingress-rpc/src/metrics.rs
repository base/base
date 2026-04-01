//! Prometheus metrics for the tips ingress RPC service.

base_metrics::define_metrics! {
    tips_ingress_rpc
    #[describe("Number of valid transactions received")]
    transactions_received: counter,
    #[describe("Number of valid bundles parsed")]
    bundles_parsed: counter,
    #[describe("Number of bundles simulated")]
    successful_simulations: counter,
    #[describe("Number of bundles that failed simulation")]
    failed_simulations: counter,
    #[describe("Number of bundles sent to kafka")]
    sent_to_kafka: counter,
    #[describe("Number of transactions sent to mempool")]
    sent_to_mempool: counter,
    #[describe("Duration of validate_tx")]
    validate_tx_duration: histogram,
    #[describe("Duration of validate_bundle")]
    validate_bundle_duration: histogram,
    #[describe("Duration of meter_bundle")]
    meter_bundle_duration: histogram,
    #[describe("Duration of send_raw_transaction")]
    send_raw_transaction_duration: histogram,
    #[describe("Total raw transactions forwarded to additional endpoint")]
    raw_tx_forwards_total: counter,
    #[describe("Number of bundles that exceeded the metering time")]
    bundles_exceeded_metering_time: counter,
    #[describe("Size of buffered meter bundle responses")]
    buffered_meter_bundle_responses_size: gauge,
    #[describe("RPC call latency")]
    #[label(rpc)]
    rpc_latency: histogram,
}
