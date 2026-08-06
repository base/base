//! Background poller for ordered safe L2 head updates.

use std::{future::Future, time::Duration};

use alloy_rpc_types_eth::BlockNumberOrTag;
use base_protocol::BlockInfo;
use base_runtime::Runtime;
use tokio::sync::mpsc;
use tracing::warn;

/// Fetches the current safe L2 head.
pub trait SafeHeadProvider: Send + Sync + 'static {
    /// Return the current safe L2 block information.
    fn safe_l2_head(
        &self,
    ) -> impl Future<Output = Result<BlockInfo, Box<dyn std::error::Error + Send + Sync>>> + Send + '_;
}

impl SafeHeadProvider for jsonrpsee::http_client::HttpClient {
    async fn safe_l2_head(&self) -> Result<BlockInfo, Box<dyn std::error::Error + Send + Sync>> {
        use base_consensus_rpc::RollupNodeApiClient;

        Ok(self.sync_status().await?.local_safe_l2.block_info)
    }
}

impl SafeHeadProvider for crate::RpcL2BlockProvider {
    async fn safe_l2_head(&self) -> Result<BlockInfo, Box<dyn std::error::Error + Send + Sync>> {
        let block =
            self.provider.get_block_by_number(BlockNumberOrTag::Safe).await?.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "safe L2 block unavailable from parity validator",
                )
            })?;
        Ok(BlockInfo {
            hash: block.header.hash,
            number: block.header.number,
            parent_hash: block.header.parent_hash,
            timestamp: block.header.timestamp,
        })
    }
}

/// Polls a provider and sends every safe-head change in observation order.
#[derive(Debug)]
pub struct SafeHeadPoller<C: SafeHeadProvider> {
    provider: C,
    poll_interval: Duration,
    last_safe_head: BlockInfo,
    safe_head_tx: mpsc::Sender<BlockInfo>,
}

impl<C: SafeHeadProvider> SafeHeadPoller<C> {
    /// Create a poller starting from the last safe head already observed.
    pub const fn new(
        provider: C,
        poll_interval: Duration,
        last_safe_head: BlockInfo,
        safe_head_tx: mpsc::Sender<BlockInfo>,
    ) -> Self {
        Self { provider, poll_interval, last_safe_head, safe_head_tx }
    }

    /// Poll until `runtime` is cancelled or the receiver closes.
    pub async fn run<R: Runtime>(mut self, runtime: R) {
        loop {
            tokio::select! {
                biased;
                _ = runtime.cancelled() => break,
                _ = runtime.sleep(self.poll_interval) => {}
            }

            match self.provider.safe_l2_head().await {
                Ok(head) if head != self.last_safe_head => {
                    let send = self.safe_head_tx.send(head);
                    tokio::select! {
                        biased;
                        _ = runtime.cancelled() => break,
                        result = send => {
                            if result.is_err() {
                                break;
                            }
                            self.last_safe_head = head;
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    warn!(%error, "failed to poll safe L2 head");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex, time::Duration};

    use alloy_primitives::B256;
    use base_protocol::BlockInfo;
    use base_runtime::{
        Cancellation, Clock, Spawner,
        deterministic::{Config, Runner},
    };
    use tokio::sync::mpsc;

    use super::{SafeHeadPoller, SafeHeadProvider};

    struct MockProvider {
        heads: Mutex<VecDeque<BlockInfo>>,
        fallback: BlockInfo,
    }

    impl SafeHeadProvider for MockProvider {
        async fn safe_l2_head(
            &self,
        ) -> Result<BlockInfo, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.heads.lock().unwrap().pop_front().unwrap_or(self.fallback))
        }
    }

    fn head(number: u64) -> BlockInfo {
        BlockInfo { hash: B256::with_last_byte(number as u8), number, ..Default::default() }
    }

    #[test]
    fn sends_ordered_head_changes_without_duplicates() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (tx, mut rx) = mpsc::channel(1);
            let provider = MockProvider {
                heads: Mutex::new(VecDeque::from([head(5), head(5), head(10), head(10)])),
                fallback: head(10),
            };
            let poller = SafeHeadPoller::new(provider, Duration::from_secs(1), head(10), tx);
            let handle = ctx.spawn(poller.run(ctx.clone()));

            assert_eq!(rx.recv().await, Some(head(5)));
            assert_eq!(rx.recv().await, Some(head(10)));
            ctx.sleep(Duration::from_secs(2)).await;
            assert!(matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
            ctx.cancel();
            handle.await.unwrap();
        });
    }
}
