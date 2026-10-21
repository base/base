//! Push-based block-production health check driven by `eth_subscribe("newHeads")`.
//!
//! The polling checker measures block age from `header.timestamp`, which is whole Unix
//! seconds, so it cannot resolve stalls shorter than about one second. This checker records
//! the local monotonic time at which each new head arrives and classifies health from the
//! time elapsed since the last one. That works at any block time, including 200 ms blocks.

use std::{
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use tokio::time::{Instant, MissedTickBehavior, interval, sleep, timeout};
use tracing::{debug, error, info, warn};

use super::{HeaderSummary, HealthState, HealthcheckConfig, Node, spawn_status_emitter};
use crate::metrics::HealthcheckMetrics;

/// Stream of new block headers produced by a [`HeadSubscriber`].
pub type HeadStream = Pin<Box<dyn Stream<Item = HeaderSummary> + Send>>;

/// Opens a new-heads subscription against an execution-layer node.
#[async_trait]
pub trait HeadSubscriber: Send + Sync {
    /// Open a fresh subscription. The returned stream ends when the subscription is lost.
    async fn subscribe(&self) -> Result<HeadStream, Box<dyn std::error::Error + Send + Sync>>;
}

/// Upper bound on connecting and issuing `eth_subscribe`.
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(5);
/// First reconnect delay after the subscription is lost or cannot be opened.
const MIN_BACKOFF: Duration = Duration::from_millis(100);
/// Maximum reconnect delay.
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// Resubscribe if no head arrives for this long, in case the subscription died silently.
/// A real stall is still reported, because resubscribing does not refresh the last head time.
const IDLE_RESUBSCRIBE: Duration = Duration::from_secs(10);

/// Mutable state shared by the subscription and evaluation loops.
#[derive(Debug)]
struct HeadState {
    /// Whether a subscription is currently open.
    connected: bool,
    /// When the current subscription was opened. Baseline for age before any head arrives.
    connected_at: Option<Instant>,
    /// Highest block number seen on the current subscription.
    last_number: Option<u64>,
    /// When the last advancing head arrived.
    last_seen: Option<Instant>,
    /// Report healthy until the node first reaches the chain tip.
    is_new_instance: bool,
}

impl HeadState {
    const fn new(is_new_instance: bool) -> Self {
        Self {
            connected: false,
            connected_at: None,
            last_number: None,
            last_seen: None,
            is_new_instance,
        }
    }

    const fn on_subscribed(&mut self, now: Instant) {
        self.connected = true;
        self.connected_at = Some(now);
        // `newHeads` never replays old heads, so the first head on a new subscription is
        // genuinely new even if its number is not above the pre-reconnect head (e.g. a reorg).
        self.last_number = None;
    }

    const fn on_disconnected(&mut self) {
        self.connected = false;
    }

    /// Record a head. Returns `true` if this head took the node out of new-instance mode.
    ///
    /// Heads that do not advance the block number (duplicates, reorg replays) do not refresh
    /// the last-seen time. `header_lag_ms` is the head's timestamp lag behind wall-clock time
    /// and is only used to decide when a new instance has caught up to the tip.
    fn on_head(&mut self, number: u64, header_lag_ms: u64, grace_ms: u64, now: Instant) -> bool {
        if self.last_number.is_some_and(|last| number <= last) {
            return false;
        }
        self.last_number = Some(number);
        self.last_seen = Some(now);
        // Header timestamps are whole seconds, so allow one extra second of rounding.
        if self.is_new_instance && header_lag_ms <= grace_ms.saturating_add(1_000) {
            self.is_new_instance = false;
            return true;
        }
        false
    }

    fn head_age(&self, now: Instant) -> Option<Duration> {
        self.last_seen.or(self.connected_at).map(|t| now.saturating_duration_since(t))
    }

    fn classify(&self, now: Instant, config: &HealthcheckConfig) -> HealthState {
        if !self.connected {
            return HealthState::Error;
        }
        if self.is_new_instance {
            return HealthState::Healthy;
        }
        let Some(age) = self.head_age(now) else {
            return HealthState::Healthy;
        };
        let age_ms = u64::try_from(age.as_millis()).unwrap_or(u64::MAX);
        if age_ms >= config.unhealthy_node_threshold_ms {
            HealthState::Unhealthy
        } else if age_ms > config.grace_period_ms {
            HealthState::Delayed
        } else {
            HealthState::Healthy
        }
    }
}

/// Block-production health checker driven by a new-heads subscription.
///
/// [`HealthcheckConfig::grace_period_ms`] and [`HealthcheckConfig::unhealthy_node_threshold_ms`]
/// are measured against the time since the last new head arrived, so they must be larger than
/// the chain's block time. [`HealthcheckConfig::poll_interval_ms`] is how often that age is
/// re-evaluated.
#[derive(Debug)]
pub struct HeadSubscriptionHealthChecker<S: HeadSubscriber> {
    node_url: String,
    subscriber: S,
    config: HealthcheckConfig,
    status_code: Arc<AtomicU8>,
    metrics: HealthcheckMetrics,
    state: Mutex<HeadState>,
}

impl<S: HeadSubscriber> HeadSubscriptionHealthChecker<S> {
    /// Create a checker. The initial state is [`HealthState::Healthy`].
    pub fn new(
        node: Node,
        subscriber: S,
        config: HealthcheckConfig,
        metrics: HealthcheckMetrics,
    ) -> Self {
        Self {
            node_url: node.url,
            subscriber,
            config,
            status_code: Arc::new(AtomicU8::new(HealthState::Healthy.code())),
            metrics,
            state: Mutex::new(HeadState::new(node.is_new_instance)),
        }
    }

    /// Spawn a background task that publishes the current [`HealthState`] to `StatsD` every
    /// `period_ms` milliseconds.
    pub fn spawn_status_emitter(&self, period_ms: u64) -> tokio::task::JoinHandle<()> {
        spawn_status_emitter(Arc::clone(&self.status_code), self.metrics.clone(), period_ms)
    }

    /// Current health state.
    pub fn status(&self) -> HealthState {
        HealthState::try_from(self.status_code.load(Ordering::Relaxed))
            .unwrap_or(HealthState::Error)
    }

    /// Run the subscription and evaluation loops forever.
    pub async fn run(&self) {
        tokio::join!(self.subscription_loop(), self.evaluation_loop());
    }

    fn lock_state(&self) -> MutexGuard<'_, HeadState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_new_instance(&self) -> bool {
        self.lock_state().is_new_instance
    }

    /// Keep a subscription open, recording every head, and reconnect with backoff on loss.
    async fn subscription_loop(&self) {
        let url = &self.node_url;
        let mut backoff = MIN_BACKOFF;
        'subscribe: loop {
            match timeout(SUBSCRIBE_TIMEOUT, self.subscriber.subscribe()).await {
                Ok(Ok(mut stream)) => {
                    info!(sequencer = %url, "subscribed to newHeads");
                    self.lock_state().on_subscribed(Instant::now());
                    backoff = MIN_BACKOFF;
                    loop {
                        match timeout(IDLE_RESUBSCRIBE, stream.next()).await {
                            Ok(Some(header)) => self.record_head(&header),
                            Ok(None) => {
                                warn!(sequencer = %url, "newHeads subscription ended");
                                break;
                            }
                            Err(_elapsed) => {
                                warn!(
                                    sequencer = %url,
                                    idle_ms = IDLE_RESUBSCRIBE.as_millis(),
                                    "no new heads received, resubscribing"
                                );
                                continue 'subscribe;
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    if self.is_new_instance() {
                        debug!(sequencer = %url, error = %e, "waiting for node to accept subscriptions");
                    } else {
                        error!(sequencer = %url, error = %e, "failed to subscribe to newHeads");
                    }
                }
                Err(_elapsed) => {
                    if self.is_new_instance() {
                        debug!(sequencer = %url, "waiting for node to accept subscriptions (timeout)");
                    } else {
                        error!(sequencer = %url, "failed to subscribe to newHeads (timeout)");
                    }
                }
            }
            self.lock_state().on_disconnected();
            sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    fn record_head(&self, header: &HeaderSummary) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let header_lag_ms =
            now_ms.saturating_sub(header.timestamp_unix_seconds.saturating_mul(1000));
        let became_healthy = self.lock_state().on_head(
            header.number,
            header_lag_ms,
            self.config.grace_period_ms,
            Instant::now(),
        );
        if became_healthy {
            info!(block_number = header.number, sequencer = %self.node_url, "node becoming healthy");
        }
        debug!(block_number = header.number, sequencer = %self.node_url, "new head");
    }

    /// Re-classify every `poll_interval_ms` and log state transitions.
    async fn evaluation_loop(&self) {
        let mut ticker = interval(Duration::from_millis(self.config.poll_interval_ms));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let now = Instant::now();
            let (state, age, last_number) = {
                let guard = self.lock_state();
                (guard.classify(now, &self.config), guard.head_age(now), guard.last_number)
            };
            let previous = self.status_code.swap(state.code(), Ordering::Relaxed);
            if previous == state.code() {
                continue;
            }
            let age_ms = age.map(|a| u64::try_from(a.as_millis()).unwrap_or(u64::MAX));
            let url = &self.node_url;
            match state {
                HealthState::Unhealthy => {
                    error!(sequencer = %url, ?age_ms, ?last_number, "block production unhealthy");
                }
                HealthState::Delayed => {
                    info!(sequencer = %url, ?age_ms, ?last_number, "delayed block production detected");
                }
                HealthState::Healthy => {
                    info!(sequencer = %url, ?last_number, "block production healthy");
                }
                // Subscription failures are logged by the subscription loop.
                HealthState::Error => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, net::UdpSocket};

    use cadence::{StatsdClient, UdpMetricSink};
    use tokio::sync::mpsc;

    use super::*;

    /// Hands out one pre-made subscription per `subscribe` call; fails once they run out.
    struct MockSubscriber {
        streams: Mutex<VecDeque<mpsc::UnboundedReceiver<HeaderSummary>>>,
    }

    impl MockSubscriber {
        fn new(count: usize) -> (Self, Vec<mpsc::UnboundedSender<HeaderSummary>>) {
            let (senders, receivers): (Vec<_>, VecDeque<_>) =
                (0..count).map(|_| mpsc::unbounded_channel()).unzip();
            (Self { streams: Mutex::new(receivers) }, senders)
        }
    }

    #[async_trait]
    impl HeadSubscriber for MockSubscriber {
        async fn subscribe(&self) -> Result<HeadStream, Box<dyn std::error::Error + Send + Sync>> {
            let rx = self.streams.lock().unwrap().pop_front().ok_or("no more subscriptions")?;
            Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|h| (h, rx))
            })))
        }
    }

    fn mock_metrics() -> HealthcheckMetrics {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let sink = UdpMetricSink::from("127.0.0.1:8125", socket).unwrap();
        HealthcheckMetrics::new(StatsdClient::from_sink("test", sink))
    }

    fn now_secs() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    fn head(number: u64, timestamp_unix_seconds: u64) -> HeaderSummary {
        HeaderSummary { number, timestamp_unix_seconds, transaction_count: 0 }
    }

    /// 200 ms blocks: delayed after 600 ms without a head, unhealthy after 1 s.
    fn sub_second_config() -> HealthcheckConfig {
        HealthcheckConfig::new(50, 600, 1_000)
    }

    fn start(
        subscriber: MockSubscriber,
        is_new_instance: bool,
    ) -> Arc<HeadSubscriptionHealthChecker<MockSubscriber>> {
        let checker = Arc::new(HeadSubscriptionHealthChecker::new(
            Node::new("ws://localhost:8546", is_new_instance),
            subscriber,
            sub_second_config(),
            mock_metrics(),
        ));
        let runner = Arc::clone(&checker);
        tokio::spawn(async move { runner.run().await });
        checker
    }

    #[test]
    fn non_advancing_heads_do_not_refresh_last_seen() {
        let t0 = Instant::now();
        let mut state = HeadState::new(false);
        state.on_subscribed(t0);
        state.on_head(10, 0, 600, t0);
        state.on_head(10, 0, 600, t0 + Duration::from_millis(500));
        state.on_head(9, 0, 600, t0 + Duration::from_millis(700));
        assert_eq!(state.last_seen, Some(t0));
        assert_eq!(
            state.classify(t0 + Duration::from_millis(800), &sub_second_config()),
            HealthState::Delayed
        );
    }

    #[test]
    fn age_uses_subscription_time_before_first_head() {
        let t0 = Instant::now();
        let mut state = HeadState::new(false);
        assert_eq!(state.classify(t0, &sub_second_config()), HealthState::Error);
        state.on_subscribed(t0);
        assert_eq!(state.classify(t0, &sub_second_config()), HealthState::Healthy);
        assert_eq!(
            state.classify(t0 + Duration::from_millis(1_000), &sub_second_config()),
            HealthState::Unhealthy
        );
    }

    #[tokio::test(start_paused = true)]
    async fn detects_sub_second_stall_and_recovery() {
        let (subscriber, senders) = MockSubscriber::new(1);
        let checker = start(subscriber, false);
        let tx = &senders[0];

        // Steady 200 ms blocks stay healthy.
        for n in 1..=10 {
            tx.send(head(n, now_secs())).unwrap();
            sleep(Duration::from_millis(200)).await;
            assert_eq!(checker.status(), HealthState::Healthy, "block {n}");
        }

        // Stall: delayed once past the 600 ms grace period, unhealthy at 1 s.
        sleep(Duration::from_millis(500)).await;
        assert_eq!(checker.status(), HealthState::Delayed);
        sleep(Duration::from_millis(400)).await;
        assert_eq!(checker.status(), HealthState::Unhealthy);

        // Production resumes.
        tx.send(head(11, now_secs())).unwrap();
        sleep(Duration::from_millis(100)).await;
        assert_eq!(checker.status(), HealthState::Healthy);
    }

    #[tokio::test(start_paused = true)]
    async fn lost_subscription_reports_error_then_resubscribes() {
        let (subscriber, mut senders) = MockSubscriber::new(2);
        let checker = start(subscriber, false);

        let first = senders.remove(0);
        first.send(head(1, now_secs())).unwrap();
        sleep(Duration::from_millis(100)).await;
        assert_eq!(checker.status(), HealthState::Healthy);

        drop(first);
        sleep(Duration::from_millis(60)).await;
        assert_eq!(checker.status(), HealthState::Error);

        // Reconnects after the first 100 ms backoff.
        sleep(Duration::from_millis(100)).await;
        senders[0].send(head(2, now_secs())).unwrap();
        sleep(Duration::from_millis(100)).await;
        assert_eq!(checker.status(), HealthState::Healthy);
    }

    #[tokio::test(start_paused = true)]
    async fn new_instance_stays_healthy_until_caught_up() {
        let (subscriber, senders) = MockSubscriber::new(1);
        let checker = start(subscriber, true);
        let tx = &senders[0];

        // Syncing: heads arrive but are a minute behind the tip.
        tx.send(head(1, now_secs() - 60)).unwrap();
        sleep(Duration::from_millis(2_000)).await;
        assert_eq!(checker.status(), HealthState::Healthy);
        assert!(checker.is_new_instance());

        // Caught up: from now on stalls are reported.
        tx.send(head(2, now_secs())).unwrap();
        sleep(Duration::from_millis(100)).await;
        assert!(!checker.is_new_instance());
        sleep(Duration::from_millis(1_000)).await;
        assert_eq!(checker.status(), HealthState::Unhealthy);
    }
}
