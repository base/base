//! Block-production health-check state machine and async polling driver.
//!
//! [`BlockProductionHealthChecker`] fetches the latest block header from an
//! Ethereum HTTP RPC endpoint on a fixed interval, classifies it into a
//! [`HealthState`], and stores the current state in an atomic that a decoupled
//! 2 s heartbeat task (`spawn_status_emitter`) publishes to `StatsD`.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use tokio::time::{interval, timeout};
use tracing::{debug, error, info};

use crate::metrics::HealthcheckMetrics;

mod alloy_client;
pub use alloy_client::AlloyEthClient;

/// The four terminal health states classified by [`BlockProductionHealthChecker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    /// Latest block age is within the grace period.
    Healthy,
    /// Latest block age is past the grace period but below the unhealthy threshold.
    Delayed,
    /// Latest block age is at or above the unhealthy threshold; block production is stalled.
    Unhealthy,
    /// The RPC call to fetch the latest block failed or timed out.
    Error,
}

impl HealthState {
    /// Numeric encoding used by the heartbeat emitter to pick which counter to increment.
    pub const fn code(&self) -> u8 {
        match self {
            Self::Healthy => 0,
            Self::Delayed => 1,
            Self::Unhealthy => 2,
            Self::Error => 3,
        }
    }
}

impl TryFrom<u8> for HealthState {
    type Error = u8;

    fn try_from(code: u8) -> Result<Self, u8> {
        match code {
            0 => Ok(Self::Healthy),
            1 => Ok(Self::Delayed),
            2 => Ok(Self::Unhealthy),
            3 => Ok(Self::Error),
            other => Err(other),
        }
    }
}

/// Timing thresholds that classify a [`HeaderSummary`] into a [`HealthState`].
#[derive(Debug, Clone)]
pub struct HealthcheckConfig {
    /// Interval at which the RPC endpoint is polled.
    pub poll_interval_ms: u64,
    /// Block ages below this are [`HealthState::Healthy`].
    pub grace_period_ms: u64,
    /// Block ages at or above this are [`HealthState::Unhealthy`].
    pub unhealthy_node_threshold_ms: u64,
}

impl HealthcheckConfig {
    /// Construct a new config with explicit thresholds.
    pub const fn new(
        poll_interval_ms: u64,
        grace_period_ms: u64,
        unhealthy_node_threshold_ms: u64,
    ) -> Self {
        Self { poll_interval_ms, grace_period_ms, unhealthy_node_threshold_ms }
    }
}

/// The RPC target of a health checker plus a bootstrap flag.
#[derive(Debug, Clone)]
pub struct Node {
    /// HTTP RPC URL of the execution-layer node.
    pub url: String,
    /// When true, delayed/unhealthy classifications are suppressed until the first Healthy
    /// observation so pod startup doesn't page.
    pub is_new_instance: bool,
}

impl Node {
    /// Construct a new `Node`.
    pub fn new(url: impl Into<String>, is_new_instance: bool) -> Self {
        Self { url: url.into(), is_new_instance }
    }
}

/// The subset of a block header that [`BlockProductionHealthChecker`] needs.
#[derive(Debug, Clone)]
pub struct HeaderSummary {
    /// Block number.
    pub number: u64,
    /// Block timestamp in Unix seconds.
    pub timestamp_unix_seconds: u64,
    /// Number of transactions in the block.
    pub transaction_count: usize,
}

/// Minimal Ethereum client interface used by [`BlockProductionHealthChecker`].
#[async_trait]
pub trait EthClient: Send + Sync {
    /// Fetch the latest block header summary.
    async fn latest_header(
        &self,
    ) -> Result<HeaderSummary, Box<dyn std::error::Error + Send + Sync>>;
}

/// Long-running poller that classifies block production into a [`HealthState`] and drives a
/// `StatsD` emitter task.
#[derive(Debug)]
pub struct BlockProductionHealthChecker<C: EthClient> {
    /// The node being monitored.
    pub node: Node,
    /// The Ethereum client used to fetch block headers.
    pub client: C,
    /// Timing thresholds.
    pub config: HealthcheckConfig,
    status_code: Arc<AtomicU8>,
    metrics: HealthcheckMetrics,
}

impl<C: EthClient> BlockProductionHealthChecker<C> {
    /// Construct a new checker starting in [`HealthState::Healthy`].
    pub fn new(
        node: Node,
        client: C,
        config: HealthcheckConfig,
        metrics: HealthcheckMetrics,
    ) -> Self {
        // default to healthy until first classification
        let initial_status: u8 = HealthState::Healthy.code();
        Self { node, client, config, status_code: Arc::new(AtomicU8::new(initial_status)), metrics }
    }

    /// Spawn a background task that publishes the current [`HealthState`] to `StatsD` every
    /// `period_ms` milliseconds, independent of poll cadence.
    pub fn spawn_status_emitter(&self, period_ms: u64) -> tokio::task::JoinHandle<()> {
        let status = Arc::clone(&self.status_code);
        let metrics = self.metrics.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(period_ms));
            loop {
                ticker.tick().await;
                let code = status.load(Ordering::Relaxed);
                let state = HealthState::try_from(code).unwrap_or(HealthState::Error);
                match state {
                    HealthState::Healthy => metrics.increment_status_healthy(),
                    HealthState::Delayed => metrics.increment_status_delayed(),
                    HealthState::Unhealthy => metrics.increment_status_unhealthy(),
                    HealthState::Error => metrics.increment_status_error(),
                }
            }
        })
    }

    /// Run a single health-check iteration: fetch latest header (2 s timeout), classify, store.
    pub async fn run_health_check(&mut self) {
        let url = &self.node.url;

        debug!(sequencer = %url, "checking block production health");

        // Enforce a 2s timeout on header fetch
        let header_result = timeout(Duration::from_secs(2), self.client.latest_header()).await;

        let latest = match header_result {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                if self.node.is_new_instance {
                    debug!(sequencer = %url, error = %e, "waiting for node to become healthy");
                } else {
                    error!(sequencer = %url, error = %e, "failed to fetch block");
                }
                self.status_code.store(HealthState::Error.code(), Ordering::Relaxed);
                return;
            }
            Err(_elapsed) => {
                if self.node.is_new_instance {
                    debug!(sequencer = %url, "waiting for node to become healthy (timeout)");
                } else {
                    error!(sequencer = %url, "failed to fetch block (timeout)");
                }
                self.status_code.store(HealthState::Error.code(), Ordering::Relaxed);
                return;
            }
        };

        // Compute block age
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_secs();
        let block_age_ms = now_secs.saturating_sub(latest.timestamp_unix_seconds) * 1000;

        let unhealthy_ms = self.config.unhealthy_node_threshold_ms;
        let grace_ms = self.config.grace_period_ms;

        let state = if self.node.is_new_instance {
            HealthState::Healthy
        } else if block_age_ms >= unhealthy_ms {
            HealthState::Unhealthy
        } else if block_age_ms > grace_ms {
            HealthState::Delayed
        } else {
            HealthState::Healthy
        };
        self.status_code.store(state.code(), Ordering::Relaxed);

        if block_age_ms > grace_ms {
            if self.node.is_new_instance {
                // Suppress delayed/unhealthy while new instance catches up
            } else if block_age_ms >= unhealthy_ms {
                error!(
                    block_number = latest.number,
                    tx_count = latest.transaction_count,
                    sequencer = %url,
                    age_ms = block_age_ms,
                    "block production unhealthy"
                );
            } else {
                info!(
                    block_number = latest.number,
                    tx_count = latest.transaction_count,
                    sequencer = %url,
                    age_ms = block_age_ms,
                    "delayed block production detected"
                );
            }
        } else {
            if self.node.is_new_instance {
                self.node.is_new_instance = false;
                info!(
                    block_number = latest.number,
                    tx_count = latest.transaction_count,
                    sequencer = %url,
                    "node becoming healthy"
                );
            }
            debug!(
                block_number = latest.number,
                tx_count = latest.transaction_count,
                sequencer = %url,
                "block production healthy"
            );
        }
    }

    /// Run [`Self::run_health_check`] in a loop on the configured poll interval, forever.
    pub async fn poll_for_health_checks(&mut self) {
        let mut ticker = interval(Duration::from_millis(self.config.poll_interval_ms));
        loop {
            ticker.tick().await;
            self.run_health_check().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::UdpSocket,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use cadence::{StatsdClient, UdpMetricSink};

    use super::*;

    #[derive(Clone)]
    struct MockClient {
        header: Arc<Mutex<HeaderSummary>>,
    }

    #[async_trait]
    impl EthClient for MockClient {
        async fn latest_header(
            &self,
        ) -> Result<HeaderSummary, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.header.lock().unwrap().clone())
        }
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_secs()
    }

    fn mock_metrics() -> HealthcheckMetrics {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let sink = UdpMetricSink::from("127.0.0.1:8125", socket).unwrap();
        let statsd_client = StatsdClient::from_sink("test", sink);
        HealthcheckMetrics::new(statsd_client)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn healthy_new_block_emits_healthy() {
        let cfg = HealthcheckConfig::new(1_000, 5_000, 15_000);
        let start = now_secs();
        let shared_header = Arc::new(Mutex::new(HeaderSummary {
            number: 1,
            timestamp_unix_seconds: start,
            transaction_count: 5,
        }));
        let client = MockClient { header: Arc::clone(&shared_header) };
        let node = Node::new("http://localhost:8545", false);
        let metrics = mock_metrics();
        let mut checker = BlockProductionHealthChecker::new(node, client, cfg, metrics);

        checker.run_health_check().await;
        assert_eq!(checker.status_code.load(Ordering::Relaxed), HealthState::Healthy.code());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delayed_new_block_emits_delayed() {
        let grace_ms = 5_000u64;
        let cfg = HealthcheckConfig::new(1_000, grace_ms, 15_000);
        let start = now_secs();
        let shared_header = Arc::new(Mutex::new(HeaderSummary {
            number: 1,
            timestamp_unix_seconds: start,
            transaction_count: 5,
        }));
        let client = MockClient { header: Arc::clone(&shared_header) };
        let node = Node::new("http://localhost:8545", false);
        let metrics = mock_metrics();
        let mut checker = BlockProductionHealthChecker::new(node, client, cfg, metrics);

        // First healthy block
        checker.run_health_check().await;
        assert_eq!(checker.status_code.load(Ordering::Relaxed), HealthState::Healthy.code());

        // Next block arrives but is delayed beyond grace
        let delayed_ts = start.saturating_sub((grace_ms / 1000) + 1);
        *shared_header.lock().unwrap() =
            HeaderSummary { number: 2, timestamp_unix_seconds: delayed_ts, transaction_count: 5 };
        checker.run_health_check().await;
        assert_eq!(checker.status_code.load(Ordering::Relaxed), HealthState::Delayed.code());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unhealthy_same_block_triggers_single_stall_emit() {
        let unhealthy_ms = 15_000u64;
        let cfg = HealthcheckConfig::new(1_000, 5_000, unhealthy_ms);
        let start = now_secs();
        let shared_header = Arc::new(Mutex::new(HeaderSummary {
            number: 10,
            timestamp_unix_seconds: start,
            transaction_count: 5,
        }));
        let client = MockClient { header: Arc::clone(&shared_header) };
        let node = Node::new("http://localhost:8545", false);
        let metrics = mock_metrics();
        let mut checker = BlockProductionHealthChecker::new(node, client, cfg, metrics);

        // First observation (healthy)
        checker.run_health_check().await;
        assert_eq!(checker.status_code.load(Ordering::Relaxed), HealthState::Healthy.code());

        // Same head, but now sufficiently old to be unhealthy -> emits stall once
        let unhealthy_ts = start.saturating_sub((unhealthy_ms / 1000) + 1);
        *shared_header.lock().unwrap() = HeaderSummary {
            number: 10,
            timestamp_unix_seconds: unhealthy_ts,
            transaction_count: 5,
        };
        checker.run_health_check().await;
        assert_eq!(checker.status_code.load(Ordering::Relaxed), HealthState::Unhealthy.code());

        // Re-run again with same head: should not re-emit; flag remains set
        checker.run_health_check().await;
        assert_eq!(checker.status_code.load(Ordering::Relaxed), HealthState::Unhealthy.code());
    }
}
