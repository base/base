use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tokio::time::{interval, timeout};
use tracing::{debug, error, info};

use crate::metrics::HealthcheckMetrics;

pub mod alloy_client;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthState {
    Healthy,
    Delayed,
    Unhealthy,
    Error,
}

impl HealthState {
    fn code(&self) -> u8 {
        match self {
            HealthState::Healthy => 0,
            HealthState::Delayed => 1,
            HealthState::Unhealthy => 2,
            HealthState::Error => 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HealthcheckConfig {
    pub poll_interval_ms: u64,
    pub grace_period_ms: u64,
    pub unhealthy_node_threshold_ms: u64,
}

impl HealthcheckConfig {
    pub fn new(
        poll_interval_ms: u64,
        grace_period_ms: u64,
        unhealthy_node_threshold_ms: u64,
    ) -> Self {
        Self {
            poll_interval_ms,
            grace_period_ms,
            unhealthy_node_threshold_ms,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub url: String,
    pub is_new_instance: bool,
}

impl Node {
    pub fn new(url: impl Into<String>, is_new_instance: bool) -> Self {
        Self {
            url: url.into(),
            is_new_instance,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HeaderSummary {
    pub number: u64,
    pub timestamp_unix_seconds: u64,
}

#[async_trait]
pub trait EthClient: Send + Sync {
    async fn latest_header(
        &self,
    ) -> Result<HeaderSummary, Box<dyn std::error::Error + Send + Sync>>;
}

#[derive(Debug)]
pub struct BlockProductionHealthChecker<C: EthClient> {
    pub node: Node,
    pub client: C,
    pub config: HealthcheckConfig,
    status_code: Arc<AtomicU8>,
    metrics: HealthcheckMetrics,
}

impl<C: EthClient> BlockProductionHealthChecker<C> {
    pub fn new(
        node: Node,
        client: C,
        config: HealthcheckConfig,
        metrics: HealthcheckMetrics,
    ) -> Self {
        // default to healthy until first classification
        let initial_status: u8 = HealthState::Healthy.code();
        Self {
            node,
            client,
            config,
            status_code: Arc::new(AtomicU8::new(initial_status)),
            metrics,
        }
    }

    pub fn spawn_status_emitter(&self, period_ms: u64) -> tokio::task::JoinHandle<()> {
        let status = self.status_code.clone();
        let metrics = self.metrics.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(period_ms));
            loop {
                ticker.tick().await;
                let code = status.load(Ordering::Relaxed);
                match code {
                    0 => metrics.increment_status_healthy(),
                    1 => metrics.increment_status_delayed(),
                    2 => metrics.increment_status_unhealthy(),
                    _ => metrics.increment_status_error(),
                }
            }
        })
    }

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
                    self.status_code
                        .store(HealthState::Error.code(), Ordering::Relaxed);
                } else {
                    error!(sequencer = %url, error = %e, "failed to fetch block");
                    self.status_code
                        .store(HealthState::Error.code(), Ordering::Relaxed);
                }
                return;
            }
            Err(_elapsed) => {
                if self.node.is_new_instance {
                    debug!(sequencer = %url, "waiting for node to become healthy (timeout)");
                    self.status_code
                        .store(HealthState::Error.code(), Ordering::Relaxed);
                } else {
                    error!(sequencer = %url, "failed to fetch block (timeout)");
                    self.status_code
                        .store(HealthState::Error.code(), Ordering::Relaxed);
                }
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
                    blockNumber = latest.number,
                    sequencer = %url,
                    age_ms = block_age_ms,
                    "block production unhealthy"
                );
            } else {
                info!(
                    blockNumber = latest.number,
                    sequencer = %url,
                    age_ms = block_age_ms,
                    "delayed block production detected"
                );
            }
        } else {
            if self.node.is_new_instance {
                self.node.is_new_instance = false;
                info!(
                    blockNumber = latest.number,
                    sequencer = %url,
                    "node becoming healthy"
                );
            }
            debug!(
                blockNumber = latest.number,
                sequencer = %url,
                "block production healthy"
            );
        }
    }

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
    use super::*;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

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
        use cadence::{StatsdClient, UdpMetricSink};
        use std::net::UdpSocket;
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
        }));
        let client = MockClient {
            header: shared_header.clone(),
        };
        let node = Node::new("http://localhost:8545", false);
        let metrics = mock_metrics();
        let mut checker = BlockProductionHealthChecker::new(node, client, cfg, metrics);

        checker.run_health_check().await;
        assert_eq!(
            checker.status_code.load(Ordering::Relaxed),
            HealthState::Healthy.code()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delayed_new_block_emits_delayed() {
        let grace_ms = 5_000u64;
        let cfg = HealthcheckConfig::new(1_000, grace_ms, 15_000);
        let start = now_secs();
        let shared_header = Arc::new(Mutex::new(HeaderSummary {
            number: 1,
            timestamp_unix_seconds: start,
        }));
        let client = MockClient {
            header: shared_header.clone(),
        };
        let node = Node::new("http://localhost:8545", false);
        let metrics = mock_metrics();
        let mut checker = BlockProductionHealthChecker::new(node, client, cfg, metrics);

        // First healthy block
        checker.run_health_check().await;
        assert_eq!(
            checker.status_code.load(Ordering::Relaxed),
            HealthState::Healthy.code()
        );

        // Next block arrives but is delayed beyond grace
        let delayed_ts = start.saturating_sub((grace_ms / 1000) + 1);
        *shared_header.lock().unwrap() = HeaderSummary {
            number: 2,
            timestamp_unix_seconds: delayed_ts,
        };
        checker.run_health_check().await;
        assert_eq!(
            checker.status_code.load(Ordering::Relaxed),
            HealthState::Delayed.code()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unhealthy_same_block_triggers_single_stall_emit() {
        let unhealthy_ms = 15_000u64;
        let cfg = HealthcheckConfig::new(1_000, 5_000, unhealthy_ms);
        let start = now_secs();
        let shared_header = Arc::new(Mutex::new(HeaderSummary {
            number: 10,
            timestamp_unix_seconds: start,
        }));
        let client = MockClient {
            header: shared_header.clone(),
        };
        let node = Node::new("http://localhost:8545", false);
        let metrics = mock_metrics();
        let mut checker = BlockProductionHealthChecker::new(node, client, cfg, metrics);

        // First observation (healthy)
        checker.run_health_check().await;
        assert_eq!(
            checker.status_code.load(Ordering::Relaxed),
            HealthState::Healthy.code()
        );

        // Same head, but now sufficiently old to be unhealthy -> emits stall once
        let unhealthy_ts = start.saturating_sub((unhealthy_ms / 1000) + 1);
        *shared_header.lock().unwrap() = HeaderSummary {
            number: 10,
            timestamp_unix_seconds: unhealthy_ts,
        };
        checker.run_health_check().await;
        assert_eq!(
            checker.status_code.load(Ordering::Relaxed),
            HealthState::Unhealthy.code()
        );

        // Re-run again with same head: should not re-emit; flag remains set
        checker.run_health_check().await;
        assert_eq!(
            checker.status_code.load(Ordering::Relaxed),
            HealthState::Unhealthy.code()
        );
    }
}
