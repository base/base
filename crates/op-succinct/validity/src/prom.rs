use metrics::describe_gauge;
use metrics_exporter_prometheus::PrometheusBuilder;
use metrics_process::Collector;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    thread,
    time::Duration,
};
use tracing::warn;

pub fn custom_gauges() {
    // Proof status gauges
    describe_gauge!(
        "succinct_current_unrequested_proofs",
        "Number of proofs currently unrequested"
    );
    describe_gauge!(
        "succinct_current_proving_proofs",
        "Number of proofs currently being proved"
    );
    describe_gauge!(
        "succinct_current_witnessgen_proofs",
        "Number of proofs currently in witness generation"
    );
    describe_gauge!(
        "succinct_current_execute_proofs",
        "Number of proofs currently being executed"
    );

    // Proposer gauges
    describe_gauge!(
        "succinct_highest_proven_contiguous_block",
        "Highest proven contiguous block"
    );
    describe_gauge!(
        "succinct_latest_contract_l2_block",
        "Latest L2 block number from the contract"
    );
    describe_gauge!(
        "succinct_l2_unsafe_head_block",
        "L2 unsafe head block number"
    );
    describe_gauge!("succinct_l2_finalized_block", "L2 finalized block number");
    describe_gauge!(
        "succinct_min_block_to_prove_to_agg",
        "Minimum block number required to prove for aggregation"
    );

    // Error gauges
    describe_gauge!("succinct_error_count", "Number of errors");
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
