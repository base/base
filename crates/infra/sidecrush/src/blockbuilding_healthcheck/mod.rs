use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::time::{interval, timeout};
use tracing::{debug, error, info};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

pub mod alloy_client;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthState {
    Healthy,
    Unhealthy,
    Error,
}

impl HealthState {
    fn code(&self) -> u8 {
        match self {
            HealthState::Healthy => 0,
            HealthState::Unhealthy => 1,
            HealthState::Error => 2,
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
    pub cached_block_number: Option<u64>,
    pub stall_emitted_for_current: bool,
    status_code: Arc<AtomicU8>,
}

impl<C: EthClient> BlockProductionHealthChecker<C> {
    pub fn new(node: Node, client: C, config: HealthcheckConfig) -> Self {
        // default to error until first successful read
        let initial_status: u8 = HealthState::Error.code();
        Self {
            node,
            client,
            config,
            cached_block_number: None,
            stall_emitted_for_current: false,
            status_code: Arc::new(AtomicU8::new(initial_status)),
        }
    }

    pub fn spawn_status_emitter(&self, period_ms: u64) -> tokio::task::JoinHandle<()> {
        let status = self.status_code.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(period_ms));
            loop {
                ticker.tick().await;
                let code = status.load(Ordering::Relaxed);
                let label = match code {
                    0 => "healthy",
                    1 => "unhealthy",
                    _ => "error",
                };
                metrics::counter!("base.blocks.status", "status" => label).increment(1);
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
                    self.status_code.store(HealthState::Error.code(), Ordering::Relaxed);
                } else {
                    error!(sequencer = %url, error = %e, "failed to fetch block");
                    metrics::counter!("base.blocks.error").increment(1);
                    self.status_code.store(HealthState::Error.code(), Ordering::Relaxed);
                }
                return;
            }
            Err(_elapsed) => {
                if self.node.is_new_instance {
                    debug!(sequencer = %url, "waiting for node to become healthy (timeout)");
                    self.status_code.store(HealthState::Error.code(), Ordering::Relaxed);
                } else {
                    error!(sequencer = %url, "failed to fetch block (timeout)");
                    metrics::counter!("base.blocks.error").increment(1);
                    self.status_code.store(HealthState::Error.code(), Ordering::Relaxed);
                }
                return;
            }
        };

        // Compute age and gauges first
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_secs();
        let block_age_ms = now_secs.saturating_sub(latest.timestamp_unix_seconds) * 1000;

        // Keep head age gauge for observability
        metrics::gauge!("base.blocks.head_age_ms").set(block_age_ms as f64);

        let grace_ms = self.config.grace_period_ms;
        let unhealthy_ms = self.config.unhealthy_node_threshold_ms;

        // Classify state and update last status (no head_state gauge)
        let state = if block_age_ms > grace_ms { HealthState::Unhealthy } else { HealthState::Healthy };
        self.status_code.store(state.code(), Ordering::Relaxed);

        // If new head: emit one initial classification and reset stall flag.
        // If same head: emit a single stall event once it crosses unhealthy, then suppress further repeats.
        if let Some(cached) = self.cached_block_number {
            if cached == latest.number {
                // Same head, evaluate for stall-at-unhealthy once (unless new instance suppression)
                if !self.node.is_new_instance
                    && !self.stall_emitted_for_current
                    && block_age_ms >= unhealthy_ms
                {
                    error!(
                        blockNumber = latest.number,
                        sequencer = %url,
                        age_ms = block_age_ms,
                        "chain stalled: crossed unhealthy threshold"
                    );
                    metrics::counter!("base.blocks.stalled_unhealthy").increment(1);
                    self.stall_emitted_for_current = true;
                }
                return;
            }
        }

        if block_age_ms > grace_ms {
            if !self.node.is_new_instance {
                if block_age_ms >= unhealthy_ms {
                    error!(
                        blockNumber = latest.number,
                        sequencer = %url,
                        age_ms = block_age_ms,
                        "block production unhealthy"
                    );
                    metrics::counter!("base.blocks.unhealthy").increment(1);
                } else {
                    info!(
                        blockNumber = latest.number,
                        sequencer = %url,
                        age_ms = block_age_ms,
                        "delayed block production detected"
                    );
                    metrics::counter!("base.blocks.delayed").increment(1);
                }
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
            metrics::counter!("base.blocks.healthy").increment(1);
        }

        // Cache number after evaluation
        self.cached_block_number = Some(latest.number);
        self.stall_emitted_for_current = false;
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
        let mut checker = BlockProductionHealthChecker::new(node, client, cfg);

        checker.run_health_check().await;
        assert_eq!(checker.cached_block_number, Some(1));
        assert!(!checker.stall_emitted_for_current);
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
        let mut checker = BlockProductionHealthChecker::new(node, client, cfg);

        // First healthy block
        checker.run_health_check().await;
        assert_eq!(checker.cached_block_number, Some(1));

        // Next block arrives but is delayed beyond grace
        let delayed_ts = start.saturating_sub((grace_ms / 1000) + 1);
        *shared_header.lock().unwrap() = HeaderSummary {
            number: 2,
            timestamp_unix_seconds: delayed_ts,
        };
        checker.run_health_check().await;
        assert_eq!(checker.cached_block_number, Some(2));
        assert!(!checker.stall_emitted_for_current);
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
        let mut checker = BlockProductionHealthChecker::new(node, client, cfg);

        // First observation (healthy)
        checker.run_health_check().await;
        assert_eq!(checker.cached_block_number, Some(10));
        assert!(!checker.stall_emitted_for_current);

        // Same head, but now sufficiently old to be unhealthy -> emits stall once
        let unhealthy_ts = start.saturating_sub((unhealthy_ms / 1000) + 1);
        *shared_header.lock().unwrap() = HeaderSummary {
            number: 10,
            timestamp_unix_seconds: unhealthy_ts,
        };
        checker.run_health_check().await;
        assert!(checker.stall_emitted_for_current);

        // Re-run again with same head: should not re-emit; flag remains set
        checker.run_health_check().await;
        assert!(checker.stall_emitted_for_current);
    }
}
