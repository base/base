//! Startup gate that waits for an isolated sequencer to rejoin the canonical chain.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_consensus_engine::{EngineClient, EngineClientError};
use base_protocol::L2BlockInfo;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

/// Waits until the execution head has left the boot head and caught up to the wall clock.
///
/// An isolated sequencer's datadir holds the private fork it built on its previous run, and that
/// fork's head can look fresh after a quick restart. Requiring the head to differ from the boot
/// head proves canonical input replaced it; requiring it to be fresh proves the node reached the
/// canonical tip.
#[derive(Debug)]
pub struct IsolatedStartupSync<E> {
    engine: Arc<E>,
    boot_head: B256,
    max_head_age: Duration,
}

impl<E: EngineClient> IsolatedStartupSync<E> {
    /// Interval between execution head polls.
    pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
    /// Consecutive synced polls required before the gate opens, so a single fresh gossip block
    /// observed mid-reorg does not end the sync.
    pub const REQUIRED_CONFIRMATIONS: u32 = 3;
    /// Maximum head age, in L2 block times, for the head to count as caught up.
    pub const MAX_HEAD_AGE_BLOCKS: u64 = 2;
    /// Polls between progress logs while the head is still syncing.
    pub const PROGRESS_LOG_POLLS: u32 = 30;

    /// Records the current execution head as the boot head.
    pub async fn capture(engine: Arc<E>) -> Result<Self, EngineClientError> {
        let boot_head = engine
            .l2_block_info_by_label(BlockNumberOrTag::Latest)
            .await?
            .map_or(B256::ZERO, |head| head.block_info.hash);
        let max_head_age =
            Duration::from_secs(engine.cfg().block_time.saturating_mul(Self::MAX_HEAD_AGE_BLOCKS));
        Ok(Self { engine, boot_head, max_head_age })
    }

    /// Returns whether `head` replaced the boot head and is at most the maximum age at `now`,
    /// measured since the Unix epoch.
    pub fn is_synced(&self, head: &L2BlockInfo, now: Duration) -> bool {
        head.block_info.hash != self.boot_head
            && now.saturating_sub(Duration::from_secs(head.block_info.timestamp))
                <= self.max_head_age
    }

    /// Polls the execution head until it is synced for [`Self::REQUIRED_CONFIRMATIONS`]
    /// consecutive polls, returning that head, or `None` if `cancellation` fires first.
    ///
    /// Poll errors are logged and retried: the gate must never open on a head it could not read.
    pub async fn wait(&self, cancellation: &CancellationToken) -> Option<L2BlockInfo> {
        let mut interval = tokio::time::interval(Self::POLL_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut confirmations = 0;
        let mut polls = 0u32;
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return None,
                _ = interval.tick() => {}
            }
            polls = polls.wrapping_add(1);
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
            match self.engine.l2_block_info_by_label(BlockNumberOrTag::Latest).await {
                Ok(Some(head)) if self.is_synced(&head, now) => {
                    confirmations += 1;
                    if confirmations >= Self::REQUIRED_CONFIRMATIONS {
                        return Some(head);
                    }
                }
                Ok(head) => {
                    confirmations = 0;
                    if polls.is_multiple_of(Self::PROGRESS_LOG_POLLS) {
                        info!(
                            target: "rollup_node",
                            head = ?head.map(|head| head.block_info.number),
                            head_age_secs = ?head.map(|head| now.as_secs().saturating_sub(head.block_info.timestamp)),
                            boot_head = %self.boot_head,
                            "Isolated sequencer still syncing to the canonical chain"
                        );
                    }
                }
                Err(error) => {
                    confirmations = 0;
                    warn!(target: "rollup_node", error = %error, "Failed to read execution head during isolated startup sync");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::B256;
    use base_common_genesis::RollupConfig;
    use base_consensus_engine::test_utils::MockEngineClient;
    use base_protocol::{BlockInfo, L2BlockInfo};
    use tokio_util::sync::CancellationToken;

    use super::IsolatedStartupSync;

    const BLOCK_TIME: u64 = 2;

    fn head(hash: u8, timestamp: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::repeat_byte(hash),
                timestamp,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn now_secs() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).expect("clock after epoch").as_secs()
    }

    async fn engine_with_head(head: L2BlockInfo) -> Arc<MockEngineClient> {
        let engine = Arc::new(MockEngineClient::new(Arc::new(RollupConfig {
            block_time: BLOCK_TIME,
            ..Default::default()
        })));
        engine.set_block_info_by_tag(BlockNumberOrTag::Latest, head).await;
        engine
    }

    fn polls(count: u32) -> Duration {
        IsolatedStartupSync::<MockEngineClient>::POLL_INTERVAL * count
    }

    #[tokio::test]
    async fn fresh_boot_head_is_not_synced() {
        let boot = head(1, now_secs());
        let sync = IsolatedStartupSync::capture(engine_with_head(boot).await).await.unwrap();

        assert!(!sync.is_synced(&boot, Duration::from_secs(now_secs())));
    }

    #[tokio::test]
    async fn stale_replacement_head_is_not_synced() {
        let now = now_secs();
        let sync =
            IsolatedStartupSync::capture(engine_with_head(head(1, now)).await).await.unwrap();

        assert!(!sync.is_synced(&head(2, now - 3 * BLOCK_TIME), Duration::from_secs(now)));
        assert!(sync.is_synced(&head(2, now - 2 * BLOCK_TIME), Duration::from_secs(now)));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_canonical_head_after_consecutive_synced_polls() {
        let engine = engine_with_head(head(1, now_secs())).await;
        let sync = IsolatedStartupSync::capture(Arc::clone(&engine)).await.unwrap();
        let canonical = head(2, now_secs());
        engine.set_block_info_by_tag(BlockNumberOrTag::Latest, canonical).await;

        let synced = tokio::time::timeout(polls(5), sync.wait(&CancellationToken::new()))
            .await
            .expect("gate should open");

        assert_eq!(synced, Some(canonical));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_stays_closed_while_head_is_the_boot_head() {
        let sync = IsolatedStartupSync::capture(engine_with_head(head(1, now_secs())).await)
            .await
            .unwrap();

        let result = tokio::time::timeout(polls(10), sync.wait(&CancellationToken::new())).await;

        assert!(result.is_err(), "gate must not open on the boot head");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_none_when_cancelled() {
        let sync = IsolatedStartupSync::capture(engine_with_head(head(1, now_secs())).await)
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert_eq!(sync.wait(&cancellation).await, None);
    }
}
