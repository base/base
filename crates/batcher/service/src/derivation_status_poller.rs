//! Background poller for ordered derivation-progress snapshots.

use std::{future::Future, time::Duration};

use base_batcher_core::DerivationStatus;
use base_runtime::Runtime;
use tokio::sync::mpsc;
use tracing::warn;

/// Fetches the derivation progress relevant to the batcher.
pub trait DerivationStatusProvider: Send + Sync + 'static {
    /// Return the safe L2 head and the L1 block derivation is processing.
    fn derivation_status(
        &self,
    ) -> impl Future<Output = Result<DerivationStatus, Box<dyn std::error::Error + Send + Sync>>>
    + Send
    + '_;
}

/// Polls a provider and sends every derivation-status change in observation order.
///
/// The statuses of a starting node, without a safe L2 head or an L1 block, are skipped: the
/// driver would take them for a safe head back at genesis or derivation back at L1 block 0.
#[derive(Debug)]
pub struct DerivationStatusPoller<C: DerivationStatusProvider> {
    provider: C,
    poll_interval: Duration,
    last_status: DerivationStatus,
    status_tx: mpsc::Sender<DerivationStatus>,
}

impl<C: DerivationStatusProvider> DerivationStatusPoller<C> {
    /// Create a poller starting from the last status already observed.
    pub const fn new(
        provider: C,
        poll_interval: Duration,
        last_status: DerivationStatus,
        status_tx: mpsc::Sender<DerivationStatus>,
    ) -> Self {
        Self { provider, poll_interval, last_status, status_tx }
    }

    /// Poll until `runtime` is cancelled or the receiver closes.
    pub async fn run<R: Runtime>(mut self, runtime: R) {
        loop {
            tokio::select! {
                biased;
                _ = runtime.cancelled() => break,
                _ = self.status_tx.closed() => break,
                _ = runtime.sleep(self.poll_interval) => {}
            }

            let result = tokio::select! {
                biased;
                _ = runtime.cancelled() => break,
                _ = self.status_tx.closed() => break,
                result = self.provider.derivation_status() => result,
            };

            match result {
                Ok(status) if status.is_from_a_starting_node() => {
                    warn!(
                        "derivation status without a safe L2 head or an L1 block, node still starting"
                    );
                }
                Ok(status) if status != self.last_status => {
                    tokio::select! {
                        biased;
                        _ = runtime.cancelled() => break,
                        result = self.status_tx.send(status) => {
                            if result.is_err() {
                                break;
                            }
                            self.last_status = status;
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    warn!(error = %error, "failed to poll derivation status");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex, time::Duration};

    use alloy_primitives::B256;
    use base_batcher_core::DerivationStatus;
    use base_protocol::BlockInfo;
    use base_runtime::{
        Cancellation, Clock, Spawner,
        deterministic::{Config, Runner},
    };
    use tokio::sync::mpsc;

    use super::{DerivationStatusPoller, DerivationStatusProvider};

    /// Scripted provider kept explicit because `mockall` cannot generate this trait's
    /// borrowed `impl Future` return without changing the production API.
    struct MockProvider {
        statuses: Mutex<VecDeque<DerivationStatus>>,
        fallback: DerivationStatus,
    }

    impl DerivationStatusProvider for MockProvider {
        async fn derivation_status(
            &self,
        ) -> Result<DerivationStatus, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.statuses.lock().unwrap().pop_front().unwrap_or(self.fallback))
        }
    }

    fn head(number: u64) -> BlockInfo {
        BlockInfo { hash: B256::with_last_byte(number as u8), number, ..Default::default() }
    }

    fn status(safe_l2: u64, current_l1: u64) -> DerivationStatus {
        DerivationStatus { safe_l2: head(safe_l2), current_l1: head(current_l1) }
    }

    #[test]
    fn sends_ordered_status_changes_without_duplicates() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (tx, mut rx) = mpsc::channel(1);
            let provider = MockProvider {
                statuses: Mutex::new(VecDeque::from([
                    status(5, 1),
                    status(5, 1),
                    status(5, 2),
                    status(10, 2),
                    status(10, 2),
                ])),
                fallback: status(10, 2),
            };
            let poller =
                DerivationStatusPoller::new(provider, Duration::from_secs(1), status(10, 0), tx);
            let handle = ctx.spawn(poller.run(ctx.clone()));

            assert_eq!(rx.recv().await, Some(status(5, 1)));
            assert_eq!(rx.recv().await, Some(status(5, 2)));
            assert_eq!(rx.recv().await, Some(status(10, 2)));
            ctx.sleep(Duration::from_secs(2)).await;
            assert!(matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
            ctx.cancel();
            handle.await.unwrap();
        });
    }

    /// The poller skips the statuses of a starting node, without a safe L2 head until its
    /// engine is bootstrapped or without an L1 block until its derivation pipeline has an
    /// origin.
    #[test]
    fn skips_the_statuses_of_a_starting_node() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (tx, mut rx) = mpsc::channel(1);
            let no_safe_head =
                DerivationStatus { safe_l2: BlockInfo::default(), current_l1: head(1) };
            let no_l1_block =
                DerivationStatus { safe_l2: head(5), current_l1: BlockInfo::default() };
            let provider = MockProvider {
                statuses: Mutex::new(VecDeque::from([no_safe_head, no_l1_block])),
                fallback: status(10, 2),
            };
            let poller =
                DerivationStatusPoller::new(provider, Duration::from_secs(1), status(5, 1), tx);
            let handle = ctx.spawn(poller.run(ctx.clone()));

            assert_eq!(rx.recv().await, Some(status(10, 2)));
            ctx.cancel();
            handle.await.unwrap();
        });
    }
}
