//! Node telemetry reporting actor.

use std::{
    collections::HashSet,
    convert::Infallible,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base_consensus_engine::EngineState;
use base_consensus_gossip::{P2pRpcRequest, PeerDump, PeerInfo};
use base_telemetry_client::{
    HttpReportSink, LatencySampler, NodeIdentity, NodeReportBuilder, TelemetryConfig, TelemetryId,
    TelemetryReporter,
};
use base_telemetry_types::{Heads, NetHealth, NodeConfigReport, NodeLayer, NodeRole};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::NodeActor;

/// How long a single p2p RPC round trip may take before the field is reported as absent.
///
/// A wedged network actor must cost telemetry one field, never a reporting cycle.
pub const P2P_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Node facts derived by [`RollupNode`] at startup rather than supplied on the command line.
///
/// [`RollupNode`]: crate::RollupNode
#[derive(Debug, Clone)]
pub struct TelemetryNodeFacts {
    /// The L2 chain ID this node follows.
    pub l2_chain_id: u64,
    /// Whether this node sequences, validates, or follows.
    pub role: NodeRole,
    /// Directory whose filesystem is measured for the disk fields, when there is one.
    pub data_dir: Option<PathBuf>,
    /// Configured maximum established connections, reported as the peer target.
    pub peer_target: Option<u32>,
}

/// The live in-process sources the telemetry actor reads each cycle.
///
/// Every value in a report comes from typed node state rather than a rendered metrics registry,
/// so a metric rename cannot silently empty the payload.
#[derive(Debug, Clone)]
pub struct TelemetrySources {
    /// Chain head state published by the engine actor.
    pub engine_state: watch::Receiver<EngineState>,
    /// Request channel into the network actor.
    pub p2p_rpc: mpsc::Sender<P2pRpcRequest>,
    /// Cancellation token shared with the rollup node.
    pub cancellation: CancellationToken,
}

/// Telemetry settings resolved for a running consensus node.
///
/// Holds what the command line settles. Everything the node derives for itself arrives
/// separately as [`TelemetryNodeFacts`].
#[derive(Debug, Clone)]
pub struct TelemetryNodeConfig {
    /// Transport, cadence, and identity-file settings.
    pub client: TelemetryConfig,
    /// Client version reported as `client.client_version`.
    pub version: String,
    /// Human-readable network name, e.g. `mainnet`.
    pub network: String,
    /// Allowlisted snapshot of the node's configuration.
    pub node_config: NodeConfigReport,
}

impl TelemetryNodeConfig {
    /// Creates telemetry settings for a node running `version` on `network`.
    pub const fn new(
        client: TelemetryConfig,
        version: String,
        network: String,
        node_config: NodeConfigReport,
    ) -> Self {
        Self { client, version, network, node_config }
    }

    /// Builds the telemetry actor, or `None` when this node will not report.
    ///
    /// Returns `None` when telemetry is switched off, when no endpoint is configured, and when
    /// the identity or the HTTP sink cannot be constructed. Every one of those is a warning and
    /// a disabled reporter rather than a startup failure: a node that cannot report telemetry is
    /// still a working node.
    pub fn actor(
        &self,
        facts: TelemetryNodeFacts,
        sources: TelemetrySources,
    ) -> Option<TelemetryActor> {
        if !self.client.is_active() {
            return None;
        }
        let endpoint = self.client.endpoint.clone()?;

        let telemetry_id = match TelemetryId::load_or_create(&self.client.id_path) {
            Ok(id) => id,
            Err(error) => {
                warn!(
                    target: "telemetry",
                    error = %error,
                    "could not establish a telemetry identity; telemetry is off for this run"
                );
                return None;
            }
        };

        let sink = match HttpReportSink::new(endpoint, self.client.request_timeout) {
            Ok(sink) => sink,
            Err(error) => {
                warn!(
                    target: "telemetry",
                    error = %error,
                    "could not build the telemetry sink; telemetry is off for this run"
                );
                return None;
            }
        };

        let identity = NodeIdentity {
            telemetry_id,
            instance_id: self.client.instance_id.clone(),
            client_version: self.version.clone(),
            l2_chain_id: facts.l2_chain_id,
            network: self.network.clone(),
            layer: NodeLayer::Consensus,
            role: facts.role,
            data_dir: facts.data_dir,
        };
        let reporter = TelemetryReporter::spawn(
            Arc::new(sink),
            self.client.retry,
            self.client.queue_capacity,
            sources.cancellation.clone(),
        );

        Some(TelemetryActor::new(
            &self.client,
            NodeReportBuilder::new(identity, self.node_config.clone()),
            reporter,
            sources,
            facts.peer_target,
        ))
    }
}

/// Actor that samples node health and periodically enqueues a report for delivery.
///
/// Two cadences share one loop. Sampling records a head-lag point and refreshes the connected
/// peer set; reporting drains the accumulated window into a payload and hands it to the
/// [`TelemetryReporter`], which owns delivery and retry.
#[derive(Debug)]
pub struct TelemetryActor {
    /// Assembles the fixed and per-cycle halves of a report.
    builder: NodeReportBuilder,
    /// Queue in front of the sink; never blocks this actor.
    reporter: TelemetryReporter,
    /// Head-lag samples accumulated since the last report.
    sampler: LatencySampler,
    /// Live node state read each cycle.
    sources: TelemetrySources,
    /// Configured maximum established connections.
    peer_target: Option<u32>,
    /// Connected peer IDs as of the last sample, used to derive churn.
    connected_peers: HashSet<String>,
    /// Peers that appeared since the last report.
    peers_joined: u32,
    /// Peers that disappeared since the last report.
    peers_left: u32,
    /// Interval between head-lag samples.
    sample_interval: Duration,
    /// Interval between reports.
    report_interval: Duration,
}

impl TelemetryActor {
    /// Creates a telemetry actor from a resolved report builder and the node's live sources.
    pub fn new(
        config: &TelemetryConfig,
        builder: NodeReportBuilder,
        reporter: TelemetryReporter,
        sources: TelemetrySources,
        peer_target: Option<u32>,
    ) -> Self {
        Self {
            builder,
            reporter,
            sampler: LatencySampler::new(config.samples_per_report()),
            sources,
            peer_target,
            connected_peers: HashSet::new(),
            peers_joined: 0,
            peers_left: 0,
            sample_interval: config.sample_interval,
            report_interval: config.report_interval,
        }
    }

    /// Records one head-lag sample and folds in peer churn since the previous sample.
    pub async fn sample(&mut self) {
        if let Some(latency_secs) = self.unsafe_head_latency_secs() {
            self.sampler.record(latency_secs);
        }
        if let Some(peers) = self.connected_peer_ids().await {
            self.fold_peer_churn(peers);
        }
    }

    /// Builds a report from the accumulated window and enqueues it for delivery.
    pub async fn report(&mut self) {
        if let Some(peers) = self.connected_peer_ids().await {
            self.fold_peer_churn(peers);
        }

        let mut heads = self.heads();
        self.sampler.drain().apply(&mut heads);
        let net = self.net_health().await;

        debug!(
            target: "telemetry",
            unsafe_block = heads.unsafe_block,
            unsafe_latency_secs = heads.unsafe_latency_secs,
            peer_count = net.peer_count,
            "enqueueing telemetry report"
        );
        self.reporter.enqueue(self.builder.build(heads, net));
    }

    /// Returns the current head numbers, without the latency fields the sampler owns.
    pub fn heads(&self) -> Heads {
        let sync_state = self.sources.engine_state.borrow().sync_state;
        Heads {
            unsafe_block: sync_state.unsafe_head().block_info.number,
            local_safe_block: Self::known_head(sync_state.local_safe_head().block_info.number),
            safe_block: Self::known_head(sync_state.safe_head().block_info.number),
            finalized_block: Self::known_head(sync_state.finalized_head().block_info.number),
            ..Default::default()
        }
    }

    /// Returns a head number the engine has actually established, or `None` at the zero value.
    ///
    /// The engine reports zero for a head it has not derived yet, which is also a real block
    /// number. Reporting `None` keeps a node that restarted minutes ago distinguishable from one
    /// genuinely sitting at genesis, which is the difference between a healthy rollout and an
    /// outage on every dashboard downstream.
    pub const fn known_head(number: u64) -> Option<u64> {
        if number == 0 { None } else { Some(number) }
    }

    /// Returns wall-clock seconds behind the unsafe head, or `None` before the engine has one.
    ///
    /// The engine publishes a zeroed head until it learns a real one, and the seconds since the
    /// unix epoch is not a head-lag measurement worth reporting.
    pub fn unsafe_head_latency_secs(&self) -> Option<f64> {
        let head = self.sources.engine_state.borrow().sync_state.unsafe_head();
        (head.block_info.number != 0).then(|| Self::seconds_since(head.block_info.timestamp))
    }

    /// Returns seconds elapsed since a block timestamp, clamped at zero.
    ///
    /// A head timestamp in the future is clock skew, not negative lag.
    pub fn seconds_since(block_timestamp: u64) -> f64 {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        now.saturating_sub(block_timestamp) as f64
    }

    /// Accumulates joins and departures against the previously observed peer set.
    ///
    /// The actor starts with an empty set, so the first fold counts the node's initial peers as
    /// joins. That is the honest reading of "peers joined since this reporter started".
    pub fn fold_peer_churn(&mut self, current: HashSet<String>) {
        self.peers_joined += current.difference(&self.connected_peers).count() as u32;
        self.peers_left += self.connected_peers.difference(&current).count() as u32;
        self.connected_peers = current;
    }

    /// Builds the network half of a report, resetting the churn counters.
    ///
    /// `advertised_ip` is deliberately left unset: the ingest server records the address it
    /// observed, which is the one that actually reached it.
    pub async fn net_health(&mut self) -> NetHealth {
        let peer_info = self.local_peer_info().await;
        NetHealth {
            peer_count: self.connected_peers.len() as u32,
            peer_target: self.peer_target,
            discovered_count: self.discovered_peer_count().await,
            peers_joined: std::mem::take(&mut self.peers_joined),
            peers_left: std::mem::take(&mut self.peers_left),
            peer_id: peer_info.as_ref().map(|info| info.peer_id.clone()),
            enr: peer_info.and_then(|info| info.enr),
            advertised_ip: None,
            gossip_error_rate: None,
            rpc_error_rate: None,
        }
    }

    /// Returns the IDs of the currently connected peers.
    pub async fn connected_peer_ids(&self) -> Option<HashSet<String>> {
        let dump: PeerDump =
            self.query_p2p(|out| P2pRpcRequest::Peers { out, connected: true }).await?;
        Some(dump.peers.into_keys().collect())
    }

    /// Returns how many peers discovery currently knows about.
    pub async fn discovered_peer_count(&self) -> Option<u32> {
        let (discovered, _gossip) = self.query_p2p(P2pRpcRequest::PeerCount).await?;
        discovered.map(|count| count as u32)
    }

    /// Returns this node's own peer identity.
    pub async fn local_peer_info(&self) -> Option<PeerInfo> {
        self.query_p2p(P2pRpcRequest::PeerInfo).await
    }

    /// Sends one p2p RPC request and awaits its reply, giving up after [`P2P_QUERY_TIMEOUT`].
    pub async fn query_p2p<T, F>(&self, request: F) -> Option<T>
    where
        T: Send,
        F: FnOnce(oneshot::Sender<T>) -> P2pRpcRequest,
    {
        let (tx, rx) = oneshot::channel();
        let request = request(tx);
        let exchange = async {
            self.sources.p2p_rpc.send(request).await.ok()?;
            rx.await.ok()
        };

        tokio::time::timeout(P2P_QUERY_TIMEOUT, exchange).await.unwrap_or_else(|_| {
            debug!(target: "telemetry", "p2p query timed out; omitting the field");
            None
        })
    }
}

#[async_trait::async_trait]
impl NodeActor for TelemetryActor {
    type StartData = ();
    type Error = Infallible;

    /// Runs the sample and report cadences until the node shuts down.
    ///
    /// This function must not return while the node is running. `spawn_and_wait!` installs a
    /// [`CancellationToken`] drop guard per actor task, so returning at all — `Ok` included —
    /// cancels every other actor and stops the node. Telemetry failures are logged and
    /// swallowed; the only exit is cancellation, at which point the node is already stopping.
    async fn start(mut self, _ctx: ()) -> Result<(), Self::Error> {
        let cancellation = self.sources.cancellation.clone();

        let mut sample_tick = tokio::time::interval(self.sample_interval);
        sample_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first report waits a full interval. Reporting immediately would publish a payload
        // assembled before the engine has a head or the network has a peer.
        let mut report_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + self.report_interval,
            self.report_interval,
        );
        report_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                _ = sample_tick.tick() => {
                    tokio::select! {
                        _ = cancellation.cancelled() => return Ok(()),
                        () = self.sample() => {}
                    }
                }
                _ = report_tick.tick() => {
                    tokio::select! {
                        _ = cancellation.cancelled() => return Ok(()),
                        () = self.report() => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use base_consensus_engine::{EngineSyncState, EngineSyncStateUpdate};
    use base_protocol::L2BlockInfo;
    use base_telemetry_client::{MockReportSink, ReportSink, ReportSinkError};
    use base_telemetry_types::PruneMode;
    use tempfile::TempDir;
    use url::Url;

    use super::*;

    /// Answers p2p RPC queries with a fixed peer set for as long as the channel stays open.
    ///
    /// Hand-rolled rather than mocked: the actor talks to the network over a channel of enum
    /// requests, not through a trait `automock` can stand in for.
    fn spawn_fake_p2p(connected: &[&str]) -> mpsc::Sender<P2pRpcRequest> {
        let connected: Vec<String> = connected.iter().map(|peer| (*peer).to_string()).collect();
        let (tx, mut rx) = mpsc::channel(16);
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                match request {
                    P2pRpcRequest::Peers { out, .. } => {
                        let mut dump = PeerDump {
                            total_connected: connected.len() as u32,
                            ..Default::default()
                        };
                        for peer in &connected {
                            dump.peers.insert(peer.clone(), PeerInfo::default());
                        }
                        let _ = out.send(dump);
                    }
                    P2pRpcRequest::PeerCount(out) => {
                        let _ = out.send((Some(42), connected.len()));
                    }
                    P2pRpcRequest::PeerInfo(out) => {
                        let _ = out.send(PeerInfo {
                            peer_id: "local-peer".to_string(),
                            enr: Some("enr:local".to_string()),
                            ..Default::default()
                        });
                    }
                    _ => {}
                }
            }
        });
        tx
    }

    /// Publishes an engine state whose unsafe head is `number` blocks in, `lag_secs` behind now.
    fn engine_state(number: u64, lag_secs: u64) -> watch::Receiver<EngineState> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let mut head = L2BlockInfo::default();
        head.block_info.number = number;
        head.block_info.timestamp = now.saturating_sub(lag_secs);
        let sync_state = EngineSyncState::default()
            .updated(EngineSyncStateUpdate { unsafe_head: Some(head), ..Default::default() });
        watch::channel(EngineState { sync_state, el_sync_finished: true }).1
    }

    fn config(id_path: PathBuf) -> TelemetryConfig {
        TelemetryConfig {
            report_interval: Duration::from_secs(10),
            sample_interval: Duration::from_secs(1),
            ..TelemetryConfig::disabled(id_path)
        }
    }

    fn node_config() -> TelemetryNodeConfig {
        TelemetryNodeConfig::new(
            config(PathBuf::from("/telemetry-id-should-not-be-written")),
            "1.2.3".to_string(),
            "devnet".to_string(),
            NodeConfigReport { prune_mode: Some(PruneMode::Archive), ..Default::default() },
        )
    }

    fn facts() -> TelemetryNodeFacts {
        TelemetryNodeFacts {
            l2_chain_id: 8453,
            role: NodeRole::Validator,
            data_dir: None,
            peer_target: Some(64),
        }
    }

    fn sources(
        engine_state: watch::Receiver<EngineState>,
        p2p_rpc: mpsc::Sender<P2pRpcRequest>,
    ) -> TelemetrySources {
        TelemetrySources { engine_state, p2p_rpc, cancellation: CancellationToken::new() }
    }

    /// Builds an actor whose sink behaves as `sink` dictates.
    fn actor(sink: MockReportSink, sources: TelemetrySources) -> TelemetryActor {
        let client = config(PathBuf::from("/unused"));
        let reporter = TelemetryReporter::spawn(
            Arc::new(sink),
            client.retry,
            client.queue_capacity,
            sources.cancellation.clone(),
        );
        let identity = NodeIdentity {
            telemetry_id: TelemetryId::generate(),
            instance_id: None,
            client_version: "1.2.3".to_string(),
            l2_chain_id: 8453,
            network: "devnet".to_string(),
            layer: NodeLayer::Consensus,
            role: NodeRole::Validator,
            data_dir: None,
        };
        TelemetryActor::new(
            &client,
            NodeReportBuilder::new(identity, NodeConfigReport::default()),
            reporter,
            sources,
            Some(64),
        )
    }

    #[tokio::test]
    async fn test_a_config_without_an_endpoint_builds_no_actor_and_mints_no_identity() {
        let dir = TempDir::new().expect("temp dir");
        let id_path = dir.path().join("telemetry-id");
        let mut telemetry = node_config();
        telemetry.client = config(id_path.clone());

        let sources = sources(engine_state(10, 2), spawn_fake_p2p(&[]));
        assert!(telemetry.actor(facts(), sources).is_none());
        assert!(
            !id_path.exists(),
            "a node that will never report must not become identifiable on disk"
        );
    }

    #[tokio::test]
    async fn test_opting_out_builds_no_actor_even_with_an_endpoint() {
        let dir = TempDir::new().expect("temp dir");
        let mut telemetry = node_config();
        telemetry.client = TelemetryConfig {
            enabled: false,
            endpoint: Some(Url::parse("http://127.0.0.1:1/v1/ingest").expect("valid url")),
            ..config(dir.path().join("telemetry-id"))
        };

        let sources = sources(engine_state(10, 2), spawn_fake_p2p(&[]));
        assert!(telemetry.actor(facts(), sources).is_none());
    }

    #[tokio::test]
    async fn test_an_endpoint_builds_an_actor_and_mints_an_identity() {
        let dir = TempDir::new().expect("temp dir");
        let id_path = dir.path().join("nested").join("telemetry-id");
        let mut telemetry = node_config();
        telemetry.client = TelemetryConfig {
            endpoint: Some(Url::parse("http://127.0.0.1:1/v1/ingest").expect("valid url")),
            ..config(id_path.clone())
        };

        let sources = sources(engine_state(10, 2), spawn_fake_p2p(&[]));
        assert!(telemetry.actor(facts(), sources).is_some());
        assert!(id_path.exists(), "the identity is minted once, on the first reporting run");
    }

    #[tokio::test]
    async fn test_peer_churn_is_measured_against_the_previous_sample() {
        let sources = sources(engine_state(10, 2), mpsc::channel(1).0);
        let mut actor = actor(MockReportSink::new(), sources);

        actor.fold_peer_churn(["a", "b"].iter().map(ToString::to_string).collect());
        assert_eq!((actor.peers_joined, actor.peers_left), (2, 0));

        actor.fold_peer_churn(["b", "c", "d"].iter().map(ToString::to_string).collect());
        assert_eq!(
            (actor.peers_joined, actor.peers_left),
            (4, 1),
            "churn accumulates across samples and is only reset when a report is built"
        );
        assert_eq!(actor.connected_peers.len(), 3);
    }

    #[tokio::test]
    async fn test_latency_is_absent_until_the_engine_has_a_head() {
        let headless =
            actor(MockReportSink::new(), sources(engine_state(0, 0), mpsc::channel(1).0));
        assert_eq!(
            headless.unsafe_head_latency_secs(),
            None,
            "a zeroed head must not be reported as decades of lag"
        );

        let synced = actor(MockReportSink::new(), sources(engine_state(10, 7), mpsc::channel(1).0));
        assert!(synced.unsafe_head_latency_secs().is_some_and(|secs| (6.0..=9.0).contains(&secs)));
    }

    #[test]
    fn test_a_head_timestamp_in_the_future_reads_as_zero_lag() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        assert_eq!(TelemetryActor::seconds_since(now + 600), 0.0);
    }

    #[tokio::test]
    async fn test_a_report_carries_heads_peers_and_identity() {
        let delivered = Arc::new(AtomicU64::new(0));
        let (report_tx, mut report_rx) = mpsc::channel(4);
        let mut sink = MockReportSink::new();
        let counter = Arc::clone(&delivered);
        sink.expect_send().returning(move |report| {
            counter.fetch_add(1, Ordering::Relaxed);
            report_tx.try_send(report.clone()).expect("test channel has room");
            Box::pin(async { Ok(()) })
        });

        let sources = sources(engine_state(1_234, 3), spawn_fake_p2p(&["peer-a", "peer-b"]));
        let mut actor = actor(sink, sources);

        actor.sample().await;
        actor.report().await;

        let report = tokio::time::timeout(Duration::from_secs(5), report_rx.recv())
            .await
            .expect("the reporter should deliver within the timeout")
            .expect("a report should be delivered");

        assert_eq!(report.heads.unsafe_block, 1_234);
        assert!(report.heads.unsafe_latency_secs >= 3.0);
        assert_eq!(report.heads.unsafe_latency_samples.len(), 1);
        assert_eq!(report.net_health.peer_count, 2);
        assert_eq!(report.net_health.peers_joined, 2);
        assert_eq!(report.net_health.peers_left, 0);
        assert_eq!(report.net_health.peer_target, Some(64));
        assert_eq!(report.net_health.discovered_count, Some(42));
        assert_eq!(report.net_health.peer_id.as_deref(), Some("local-peer"));
        assert_eq!(report.net_health.enr.as_deref(), Some("enr:local"));
        assert!(report.is_current_schema());
        assert_eq!(delivered.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_churn_counters_reset_once_reported() {
        let mut sink = MockReportSink::new();
        sink.expect_send().returning(|_| Box::pin(async { Ok(()) }));

        let sources = sources(engine_state(10, 1), spawn_fake_p2p(&["peer-a"]));
        let mut actor = actor(sink, sources);

        actor.sample().await;
        let first = actor.net_health().await;
        assert_eq!(first.peers_joined, 1);

        let second = actor.net_health().await;
        assert_eq!(
            (second.peers_joined, second.peers_left),
            (0, 0),
            "churn is per-report, so a second report must not re-count the same joins"
        );
    }

    /// A sink that fails every call must not take the node down with it.
    ///
    /// `spawn_and_wait!` installs a cancellation drop guard per actor task, so a telemetry actor
    /// that returns — for any reason, including `Ok` — stops every other actor. This test is the
    /// guard on that: the actor has to outlive a totally dead endpoint.
    #[tokio::test(start_paused = true)]
    async fn test_a_failing_sink_never_stops_the_actor() {
        let attempts = Arc::new(AtomicU64::new(0));
        let (attempt_tx, mut attempt_rx) = mpsc::channel(64);
        let counter = Arc::clone(&attempts);
        let mut sink = MockReportSink::new();
        sink.expect_send().returning(move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
            let _ = attempt_tx.try_send(());
            Box::pin(async { Err(ReportSinkError::Rejected("endpoint is gone".to_string())) })
        });

        let sources = sources(engine_state(10, 1), spawn_fake_p2p(&["peer-a"]));
        let cancellation = sources.cancellation.clone();
        let handle = tokio::spawn(actor(sink, sources).start(()));

        // The clock is paused and auto-advances whenever every task is idle, so this waits for
        // several report intervals of virtual time without sleeping for real.
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(120), attempt_rx.recv())
                .await
                .expect("the actor should keep reporting into a dead endpoint")
                .expect("the sink should still be attached");
            assert!(!handle.is_finished(), "the actor must survive an endpoint that never answers");
        }
        assert!(attempts.load(Ordering::Relaxed) >= 3, "every cycle should attempt delivery");

        cancellation.cancel();
        handle.await.expect("the actor task should not panic").expect("the actor cannot fail");
    }

    #[tokio::test(start_paused = true)]
    async fn test_cancellation_stops_the_actor() {
        let mut sink = MockReportSink::new();
        sink.expect_send().returning(|_| Box::pin(async { Ok(()) }));

        let sources = sources(engine_state(10, 1), spawn_fake_p2p(&[]));
        let cancellation = sources.cancellation.clone();
        let handle = tokio::spawn(actor(sink, sources).start(()));

        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("a cancelled actor should stop promptly")
            .expect("the actor task should not panic")
            .expect("the actor cannot fail");
    }

    /// The mocked sink is only ever used through the trait, so keep the import honest.
    #[tokio::test]
    async fn test_mock_sink_implements_the_trait() {
        let mut sink = MockReportSink::new();
        sink.expect_send().returning(|_| Box::pin(async { Ok(()) }));
        let sink: &dyn ReportSink = &sink;
        assert!(sink.send(&Default::default()).await.is_ok());
    }
}
