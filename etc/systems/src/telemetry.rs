//! In-process node telemetry ingest for system test stacks.
//!
//! Starts the real [`IngestRoutes`] router on an ephemeral loopback port, points the client
//! consensus node's telemetry endpoint at it, and hands the reports it accepts back to the test.
//! The whole loop — actor, HTTP transport, ingest handler, wire schema — runs unmocked.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use base_consensus_node::TelemetryNodeConfig;
use base_telemetry_client::TelemetryConfig;
use base_telemetry_service::{IngestRoutes, NODE_REPORT_PATH, ReportRecorder};
use base_telemetry_types::{NodeConfigReport, NodeReportEvent};
use base_trusted_proxy::TrustedProxyConfig;
use eyre::{OptionExt, Result, WrapErr};
use tempfile::TempDir;
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use url::Url;

/// Report cadence used when a test does not ask for one.
///
/// Short enough that a report lands inside a normal system test's lifetime, and the production
/// default of fifteen minutes would never fire at all.
const DEFAULT_REPORT_INTERVAL: Duration = Duration::from_secs(3);
/// Lag sampling cadence used when a test does not ask for one.
const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// Header the ingest router reads a forwarded client IP from.
///
/// Nothing sits in front of the test router, so this only has to be a valid header name.
const CLIENT_IP_HEADER: &str = "x-forwarded-for";

/// Options for enabling node telemetry on a system test stack.
///
/// When set on [`SystemTestStackBuilder`](crate::SystemTestStackBuilder), the stack starts an
/// ingest endpoint and configures the client consensus node to report to it.
#[derive(Debug, Clone, Copy)]
pub struct TelemetryStackOptions {
    /// How often the node builds and sends a report.
    pub report_interval: Duration,
    /// How often the node samples head lag between reports.
    pub sample_interval: Duration,
}

impl Default for TelemetryStackOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetryStackOptions {
    /// Creates telemetry options with test-scale reporting and sampling intervals.
    pub const fn new() -> Self {
        Self { report_interval: DEFAULT_REPORT_INTERVAL, sample_interval: DEFAULT_SAMPLE_INTERVAL }
    }

    /// Sets how often the node sends a report.
    pub const fn with_report_interval(mut self, interval: Duration) -> Self {
        self.report_interval = interval;
        self
    }

    /// Sets how often the node samples head lag between reports.
    pub const fn with_sample_interval(mut self, interval: Duration) -> Self {
        self.sample_interval = interval;
        self
    }

    /// Starts the ingest endpoint these options describe.
    pub async fn start(self) -> Result<TelemetryIngest> {
        TelemetryIngest::start(self).await
    }
}

/// Forwards every accepted report to the test.
///
/// Hand-rolled rather than `automock`ed because the test consumes reports as a stream over the
/// life of the stack, which is a channel, not a call expectation.
#[derive(Debug)]
pub struct ChannelRecorder {
    reports: mpsc::UnboundedSender<NodeReportEvent>,
}

impl ReportRecorder for ChannelRecorder {
    fn record(&self, event: &NodeReportEvent) {
        // The stack drops the receiver on teardown, and a report arriving after that is not an
        // error: the node is still running while the test winds down.
        let _ = self.reports.send(event.clone());
    }
}

/// A running telemetry ingest endpoint and the reports it has accepted.
#[derive(Debug)]
pub struct TelemetryIngest {
    endpoint: Url,
    options: TelemetryStackOptions,
    reports: Mutex<mpsc::UnboundedReceiver<NodeReportEvent>>,
    /// Holds the node identity file for the stack's lifetime, so a system test never writes a
    /// `telemetry-id` into the developer's home directory.
    id_dir: TempDir,
    server: JoinHandle<()>,
}

impl Drop for TelemetryIngest {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl TelemetryIngest {
    /// Binds an ephemeral loopback port and serves the ingest router on it.
    pub async fn start(options: TelemetryStackOptions) -> Result<Self> {
        let (reports, receiver) = mpsc::unbounded_channel();
        let recorder = Arc::new(ChannelRecorder { reports });
        let proxy = Arc::new(TrustedProxyConfig::new(CLIENT_IP_HEADER.to_string(), Vec::new()));
        let router = IngestRoutes::router(recorder, proxy);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .wrap_err("Failed to bind the telemetry ingest listener")?;
        let address =
            listener.local_addr().wrap_err("Failed to read the telemetry ingest address")?;
        let server = tokio::spawn(async move {
            let service = router.into_make_service_with_connect_info::<SocketAddr>();
            if let Err(error) = axum::serve(listener, service).await {
                tracing::warn!(target: "telemetry", %error, "telemetry ingest server stopped");
            }
        });

        Ok(Self {
            endpoint: Url::parse(&format!("http://{address}{NODE_REPORT_PATH}"))
                .wrap_err("Failed to build the telemetry ingest endpoint URL")?,
            options,
            reports: Mutex::new(receiver),
            id_dir: TempDir::new().wrap_err("Failed to create the telemetry identity directory")?,
            server,
        })
    }

    /// Returns the URL nodes POST reports to.
    pub const fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Returns the path the reporting node persists its telemetry identity to.
    pub fn id_path(&self) -> PathBuf {
        self.id_dir.path().join("telemetry-id")
    }

    /// Builds the telemetry configuration for a consensus node reporting to this endpoint.
    pub fn node_config(&self, version: String, network: String) -> TelemetryNodeConfig {
        let client = TelemetryConfig {
            report_interval: self.options.report_interval,
            sample_interval: self.options.sample_interval,
            ..TelemetryConfig::new(self.id_path(), Some(self.endpoint.clone()))
        };
        let node_config = NodeConfigReport {
            prune_mode: None,
            p2p_enabled: true,
            discovery_enabled: false,
            sequencer_enabled: false,
            supervisor_enabled: false,
            flashblocks_enabled: false,
            metrics_enabled: false,
            experimental_flags: Vec::new(),
            report_interval_secs: self.options.report_interval.as_secs(),
            sample_interval_secs: self.options.sample_interval.as_secs(),
        };
        TelemetryNodeConfig::new(client, version, network, node_config)
    }

    /// Waits for the next report the node sends, up to `timeout`.
    ///
    /// # Errors
    ///
    /// Returns an error if no report arrives in time, or if the ingest endpoint has shut down.
    pub async fn next_report(&self, timeout: Duration) -> Result<NodeReportEvent> {
        let mut reports = self.reports.lock().await;
        tokio::time::timeout(timeout, reports.recv())
            .await
            .wrap_err_with(|| format!("No telemetry report arrived within {timeout:?}"))?
            .ok_or_eyre("The telemetry ingest endpoint shut down")
    }

    /// Waits up to `timeout` for a report satisfying `predicate`, discarding earlier ones.
    ///
    /// The first report a node sends is built one interval after startup, when the chain may
    /// still be at genesis and peers may not have connected. Tests that assert on live values
    /// wait for a report that carries them rather than for the first one to arrive.
    ///
    /// # Errors
    ///
    /// Returns an error if no matching report arrives in time.
    pub async fn next_report_matching(
        &self,
        timeout: Duration,
        predicate: impl Fn(&NodeReportEvent) -> bool,
    ) -> Result<NodeReportEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_eyre("No matching telemetry report arrived before the deadline")?;
            let event = self.next_report(remaining).await?;
            if predicate(&event) {
                return Ok(event);
            }
        }
    }
}
