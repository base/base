//! Follows the consensus node's sequencer leadership so leader-only work can be gated on it.
//!
//! Only the active sequencer builds blocks, so its transaction events are the ones worth
//! journaling; every other builder would journal the same forwarded transactions again. The
//! probe here asks the local consensus node `admin_sequencerActive` and opens the transaction
//! event gate only while the answer is `true`. Leadership is read from the consensus node rather
//! than configured, because it moves between sequencers at runtime without a redeploy.

use core::time::Duration;

use alloy_rpc_client::RpcClient;
use base_observability_events::TransactionEventGate;
use tokio::{task::JoinHandle, time::MissedTickBehavior};
use tracing::{info, warn};
use url::Url;

use crate::BuilderMetrics;

/// Where and how often to ask the consensus node whether this sequencer is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencerLeadershipConfig {
    /// HTTP RPC endpoint of the consensus node serving `admin_sequencerActive`.
    pub rpc_url: Url,
    /// Time between two `admin_sequencerActive` requests.
    pub poll_interval: Duration,
    /// How long one request may take before it counts as failed.
    pub request_timeout: Duration,
}

impl SequencerLeadershipConfig {
    /// Default time between two `admin_sequencerActive` requests. Leadership moves rarely, and
    /// the request is answered locally, so this bounds the events leaked or lost around a
    /// transfer to a few blocks.
    pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
    /// Default time one `admin_sequencerActive` request may take.
    pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

    /// Probes `rpc_url` at the default interval and timeout.
    pub const fn new(rpc_url: Url) -> Self {
        Self {
            rpc_url,
            poll_interval: Self::DEFAULT_POLL_INTERVAL,
            request_timeout: Self::DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

/// Polls `admin_sequencerActive` on the consensus node and opens the transaction event gate
/// only while this sequencer is active.
///
/// The gate is left as found until the first answer arrives. A request that fails or times out
/// keeps the last answer, so a consensus node that stops answering does not flip the gate; the
/// failures are counted in `base_builder_sequencer_leadership_poll_errors_total` and the first
/// one of a streak is logged. Dropping the probe stops polling and leaves the gate as it is.
#[derive(Debug)]
pub struct SequencerLeadershipProbe {
    task: JoinHandle<()>,
}

impl SequencerLeadershipProbe {
    /// Starts polling on the current Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error when `config.poll_interval` is zero, which would make the probe spin
    /// on the consensus node.
    pub fn spawn(
        config: SequencerLeadershipConfig,
        gate: TransactionEventGate,
    ) -> eyre::Result<Self> {
        if config.poll_interval.is_zero() {
            eyre::bail!("sequencer leadership poll interval must not be zero");
        }
        let client = RpcClient::builder().http(config.rpc_url.clone());
        let task = tokio::spawn(Self::run(config, client, gate));
        Ok(Self { task })
    }

    async fn run(config: SequencerLeadershipConfig, client: RpcClient, gate: TransactionEventGate) {
        let mut interval = tokio::time::interval(config.poll_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_answer: Option<bool> = None;
        let mut failing = false;
        loop {
            interval.tick().await;
            let request = client.request::<_, bool>("admin_sequencerActive", ());
            let answer = match tokio::time::timeout(config.request_timeout, request).await {
                Ok(Ok(active)) => active,
                Ok(Err(error)) => {
                    Self::record_failure(&mut failing, &config.rpc_url, &error.to_string());
                    continue;
                }
                Err(_) => {
                    Self::record_failure(&mut failing, &config.rpc_url, "request timed out");
                    continue;
                }
            };
            if failing {
                failing = false;
                info!(rpc_url = %config.rpc_url, "sequencer leadership probe recovered");
            }
            if last_answer != Some(answer) {
                info!(
                    active = answer,
                    "sequencer leadership changed; transaction event gate follows"
                );
                gate.set_open(answer);
                BuilderMetrics::sequencer_active().set(f64::from(u8::from(answer)));
                last_answer = Some(answer);
            }
        }
    }

    fn record_failure(failing: &mut bool, rpc_url: &Url, error: &str) {
        BuilderMetrics::sequencer_leadership_poll_errors_total().increment(1);
        if !*failing {
            *failing = true;
            warn!(
                rpc_url = %rpc_url,
                error = %error,
                "could not read sequencer leadership; keeping the last known answer"
            );
        }
    }
}

impl Drop for SequencerLeadershipProbe {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use jsonrpsee::{
        server::{Server, ServerHandle},
        types::ErrorObjectOwned,
    };

    use super::*;

    /// A consensus node whose `admin_sequencerActive` answer the test controls. `None` answers
    /// an error, as a node whose admin API is unavailable does.
    struct FakeConsensusNode {
        url: Url,
        active: Arc<Mutex<Option<bool>>>,
        _server: ServerHandle,
    }

    impl FakeConsensusNode {
        async fn start(active: Option<bool>) -> Self {
            let active = Arc::new(Mutex::new(active));
            let mut module = jsonrpsee::RpcModule::new(Arc::clone(&active));
            module
                .register_method("admin_sequencerActive", |_, active, _| {
                    active.lock().unwrap().ok_or_else(|| {
                        ErrorObjectOwned::owned(-32601, "admin API unavailable", None::<()>)
                    })
                })
                .unwrap();
            let server = Server::builder().build("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", server.local_addr().unwrap()).parse().unwrap();
            Self { url, active, _server: server.start(module) }
        }

        fn answer(&self, active: Option<bool>) {
            *self.active.lock().unwrap() = active;
        }

        fn config(&self) -> SequencerLeadershipConfig {
            SequencerLeadershipConfig {
                rpc_url: self.url.clone(),
                poll_interval: Duration::from_millis(10),
                request_timeout: Duration::from_millis(500),
            }
        }
    }

    async fn wait_until(gate: &TransactionEventGate, open: bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while gate.is_open() != open {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("gate did not become open={open}"));
    }

    #[tokio::test]
    async fn gate_follows_the_consensus_node_answer() {
        let node = FakeConsensusNode::start(Some(false)).await;
        let gate = TransactionEventGate::default();
        let _probe = SequencerLeadershipProbe::spawn(node.config(), gate.clone()).unwrap();

        wait_until(&gate, false).await;

        node.answer(Some(true));
        wait_until(&gate, true).await;

        node.answer(Some(false));
        wait_until(&gate, false).await;
    }

    #[tokio::test]
    async fn failed_requests_keep_the_last_answer() {
        let node = FakeConsensusNode::start(None).await;
        let gate = TransactionEventGate::default();
        let _probe = SequencerLeadershipProbe::spawn(node.config(), gate.clone()).unwrap();

        // Errors before any answer leave the gate as found.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(gate.is_open());

        node.answer(Some(false));
        wait_until(&gate, false).await;

        // Errors after an answer keep that answer.
        node.answer(None);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!gate.is_open());
    }

    #[tokio::test]
    async fn zero_poll_interval_is_rejected() {
        let node = FakeConsensusNode::start(Some(false)).await;
        let gate = TransactionEventGate::default();
        let config = SequencerLeadershipConfig { poll_interval: Duration::ZERO, ..node.config() };

        let error = SequencerLeadershipProbe::spawn(config, gate.clone()).unwrap_err();
        assert!(error.to_string().contains("poll interval"), "{error}");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(gate.is_open(), "a rejected probe must not touch the gate");
    }

    #[tokio::test]
    async fn dropping_the_probe_stops_polling() {
        let node = FakeConsensusNode::start(Some(false)).await;
        let gate = TransactionEventGate::default();
        let probe = SequencerLeadershipProbe::spawn(node.config(), gate.clone()).unwrap();
        wait_until(&gate, false).await;

        drop(probe);
        node.answer(Some(true));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!gate.is_open(), "a dropped probe must not keep driving the gate");
    }
}
