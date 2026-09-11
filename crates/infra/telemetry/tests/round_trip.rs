//! Round trip between the real telemetry client and the real ingest service.
//!
//! The in-crate ingest tests hand-build a `NodeReport` and POST it with `reqwest`, which proves
//! the handler but not that the client and the service agree. These tests drive the actual
//! [`NodeReportBuilder`] and [`TelemetryReporter`] against a live [`IngestRoutes`] router, so a
//! field the client renames or a shape the service cannot parse fails here rather than in the
//! Docker-backed system test.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use base_retry::RetryConfig;
use base_telemetry_client::{
    HttpReportSink, NodeIdentity, NodeReportBuilder, ReportSink, TelemetryId, TelemetryReporter,
};
use base_telemetry_service::{IngestRoutes, NODE_REPORT_PATH, ReportRecorder};
use base_telemetry_types::{
    Heads, NODE_REPORT_SCHEMA_VERSION, NetHealth, NodeConfigReport, NodeLayer, NodeReport,
    NodeReportEvent, NodeRole, PruneMode,
};
use base_trusted_proxy::TrustedProxyConfig;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use url::Url;

/// How long to wait for an asynchronously delivered report to reach the recorder.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
/// Interval between recorder polls while awaiting delivery.
const DELIVERY_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Collects accepted events in memory.
///
/// Hand-rolled rather than `automock`ed because the assertions read the accumulated events,
/// not call counts on the trait.
#[derive(Debug, Default)]
struct CapturingRecorder {
    events: Mutex<Vec<NodeReportEvent>>,
}

impl CapturingRecorder {
    fn events(&self) -> Vec<NodeReportEvent> {
        self.events.lock().expect("recorder lock poisoned").clone()
    }
}

impl ReportRecorder for CapturingRecorder {
    fn record(&self, event: &NodeReportEvent) {
        self.events.lock().expect("recorder lock poisoned").push(event.clone());
    }
}

/// A running ingest endpoint and the events it has accepted.
struct Ingest {
    endpoint: Url,
    recorder: Arc<CapturingRecorder>,
    server: JoinHandle<()>,
}

impl Drop for Ingest {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Ingest {
    /// Serves the real ingest router on an ephemeral loopback port.
    async fn start() -> Self {
        let recorder = Arc::new(CapturingRecorder::default());
        let proxy = Arc::new(TrustedProxyConfig::new("x-forwarded-for".to_string(), Vec::new()));
        let router = IngestRoutes::router(Arc::clone(&recorder) as Arc<dyn ReportRecorder>, proxy);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let address = listener.local_addr().expect("read bound address");
        let server = tokio::spawn(async move {
            let service = router.into_make_service_with_connect_info::<SocketAddr>();
            let _ = axum::serve(listener, service).await;
        });

        Self {
            endpoint: Url::parse(&format!("http://{address}{NODE_REPORT_PATH}"))
                .expect("valid endpoint URL"),
            recorder,
            server,
        }
    }

    /// Waits for the recorder to hold at least `count` events.
    async fn wait_for_events(&self, count: usize) -> Vec<NodeReportEvent> {
        let deadline = tokio::time::Instant::now() + DELIVERY_TIMEOUT;
        loop {
            let events = self.recorder.events();
            if events.len() >= count {
                return events;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "only {} of {count} reports reached the recorder",
                events.len()
            );
            tokio::time::sleep(DELIVERY_POLL_INTERVAL).await;
        }
    }
}

/// Builds a report the way a running consensus node does.
fn report() -> NodeReport {
    let identity = NodeIdentity {
        telemetry_id: TelemetryId::generate(),
        instance_id: Some("round-trip".to_string()),
        client_version: "1.2.3".to_string(),
        l2_chain_id: 8453,
        network: "mainnet".to_string(),
        layer: NodeLayer::Consensus,
        role: NodeRole::Validator,
        data_dir: None,
    };
    let node_config = NodeConfigReport {
        prune_mode: Some(PruneMode::Archive),
        p2p_enabled: true,
        discovery_enabled: true,
        sequencer_enabled: false,
        supervisor_enabled: false,
        flashblocks_enabled: false,
        metrics_enabled: false,
        experimental_flags: vec!["round-trip".to_string()],
        report_interval_secs: 900,
        sample_interval_secs: 60,
    };
    let heads = Heads {
        unsafe_block: 1_234,
        local_safe_block: Some(1_232),
        safe_block: Some(1_230),
        finalized_block: Some(1_200),
        unsafe_latency_secs: 1.5,
        worst_unsafe_latency_secs: 4.25,
        unsafe_latency_samples: vec![0.5, 1.5, 4.25],
    };
    let net = NetHealth {
        peer_count: 17,
        peer_target: Some(64),
        peers_joined: 3,
        peers_left: 1,
        peer_id: Some("16Uiu2HAm".to_string()),
        gossip_error_rate: Some(0.0),
        ..Default::default()
    };
    NodeReportBuilder::new(identity, node_config).build(heads, net)
}

/// A report the client builds survives the wire and arrives at the recorder unchanged.
#[tokio::test]
async fn test_a_client_built_report_arrives_at_the_recorder_intact() {
    let ingest = Ingest::start().await;
    let sent = report();

    let sink = HttpReportSink::new(ingest.endpoint.clone(), Duration::from_secs(5))
        .expect("build the HTTP sink");
    sink.send(&sent).await.expect("the ingest endpoint should accept a well-formed report");

    let events = ingest.wait_for_events(1).await;
    let received = &events[0].report;

    assert_eq!(received, &sent, "every field must survive serialization and parsing");
    assert_eq!(received.schema_version, NODE_REPORT_SCHEMA_VERSION);
    assert!(events[0].reported_ip.is_loopback(), "the observed IP falls back to the connection");
}

/// The reporter's background delivery task reaches the ingest endpoint.
///
/// This is the path a node actually uses: the actor enqueues and moves on, and delivery happens
/// off the reporting cycle.
#[tokio::test]
async fn test_the_reporter_delivers_queued_reports_to_ingest() {
    let ingest = Ingest::start().await;
    let cancellation = CancellationToken::new();
    let sink = Arc::new(
        HttpReportSink::new(ingest.endpoint.clone(), Duration::from_secs(5))
            .expect("build the HTTP sink"),
    );
    let reporter = TelemetryReporter::spawn(sink, RetryConfig::default(), 4, cancellation.clone());

    let first = report();
    let second = report();
    assert!(reporter.enqueue(first.clone()), "the queue starts empty");
    assert!(reporter.enqueue(second.clone()), "the queue has room for a second report");

    let events = ingest.wait_for_events(2).await;
    assert_eq!(events[0].report, first);
    assert_eq!(events[1].report, second);
    assert_eq!(reporter.dropped_reports(), 0, "nothing should have been dropped");

    cancellation.cancel();
}
