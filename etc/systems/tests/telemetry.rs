//! End-to-end system test for node telemetry.
//!
//! Runs the whole loop unmocked: the client consensus node's telemetry actor reads live engine
//! and p2p state, builds a `node_report`, POSTs it over HTTP to the stack's ingest endpoint, and
//! the ingest handler parses it back into the shared wire type. Nothing here asserts against a
//! test double, so a schema drift between the client and the service fails this test.

use std::time::Duration;

use base_system_tests::{SystemTestStackBuilder, TelemetryStackOptions};
use base_telemetry_types::{NODE_REPORT_SCHEMA_VERSION, NodeLayer, NodeRole};
use eyre::Result;

/// L2 chain ID for this test binary, distinct from every other system test's.
const L2_CHAIN_ID: u64 = 84_538_471;
/// How often the node under test reports.
const REPORT_INTERVAL: Duration = Duration::from_secs(3);
/// How long to wait for a report carrying live chain and peer values.
///
/// Generous relative to the reporting interval: the first report is built before the chain has
/// necessarily advanced, so this has to cover several cycles.
const REPORT_TIMEOUT: Duration = Duration::from_secs(90);

/// A validator reports live head, peer, and hardware values to the ingest endpoint.
#[tokio::test]
async fn test_a_validator_reports_live_heads_and_peers() -> Result<()> {
    let system = SystemTestStackBuilder::new()
        .with_l2_chain_id(L2_CHAIN_ID)
        .with_telemetry(TelemetryStackOptions::new().with_report_interval(REPORT_INTERVAL))
        .build()
        .await?;

    // The first cycle can fire before the chain advances or the p2p handshake completes, and a
    // report of zeroes at startup is correct behavior rather than a failure.
    let event = system
        .telemetry()
        .next_report_matching(REPORT_TIMEOUT, |event| {
            event.report.heads.unsafe_block > 0 && event.report.net_health.peer_count > 0
        })
        .await?;
    let report = &event.report;

    assert_eq!(report.schema_version, NODE_REPORT_SCHEMA_VERSION);
    assert_eq!(report.client.layer, NodeLayer::Consensus);
    assert_eq!(report.client.role, NodeRole::Validator);
    assert_eq!(report.client.l2_chain_id, L2_CHAIN_ID);
    assert_eq!(report.config.report_interval_secs, REPORT_INTERVAL.as_secs());

    assert!(report.heads.unsafe_block > 0, "the validator should have followed the chain");
    assert!(
        Some(report.heads.unsafe_block) >= report.heads.safe_block,
        "the safe head can never lead the unsafe head"
    );
    assert!(
        report.net_health.peer_count > 0,
        "the validator is connected to the builder consensus node"
    );
    assert!(
        report.hardware.cpu_cores.is_some_and(|cores| cores > 0),
        "hardware collection should report a core count"
    );
    assert!(!report.telemetry_id.is_nil(), "every report carries a minted identity");

    // The node reports over loopback and advertises no routable address, so ingest falls back
    // to the observed edge IP.
    assert!(event.reported_ip.is_loopback(), "the report arrived over loopback");

    // Reporting is a loop, not a one-shot. The second report proves the actor survives a
    // completed cycle, and that the identity is stable across cycles rather than reminted.
    let next = system.telemetry().next_report(REPORT_TIMEOUT).await?;
    assert_eq!(
        next.report.telemetry_id, report.telemetry_id,
        "the telemetry identity must be stable across reports"
    );
    assert!(
        next.report.heads.unsafe_block >= report.heads.unsafe_block,
        "the reported unsafe head must not go backwards"
    );
    assert!(
        next.report.client.uptime_secs >= report.client.uptime_secs,
        "uptime must advance across reports"
    );

    Ok(())
}
