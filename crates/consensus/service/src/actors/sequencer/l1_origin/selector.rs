//! The [`L1OriginSelector`] and its next-origin state machine.

use std::{fmt::Debug, sync::Arc, time::Duration};

use alloy_primitives::B256;
use alloy_transport::{RpcError, TransportErrorKind};
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_protocol::{BlockInfo, L2BlockInfo};
use tokio::{sync::watch, time::timeout};
use tokio_util::task::AbortOnDropHandle;

use super::{L1OriginSelectorProvider, LinkedOrigin, PreparedL1Origin};
use crate::Metrics;

/// The speculative successor to `current`, and any in-flight work to obtain it.
///
/// A single enum instead of parallel `Option` fields so illegal combinations (a ready `next` *and*
/// an in-flight fetch) are unrepresentable.
#[derive(Debug, Default)]
enum NextSlot {
    /// No successor known and no fetch running. The next `select_origins` may start one.
    #[default]
    Idle,
    /// A background fetch is running, keyed to the `current.hash` it links against so a result that
    /// arrives after `current` advanced is discarded rather than trusted. The
    /// [`AbortOnDropHandle`] aborts the task if the slot is dropped or replaced.
    InFlight {
        /// The `current.hash` this fetch was started against.
        parent_hash: B256,
        /// The self-aborting handle to the background fetch.
        handle: AbortOnDropHandle<Option<PreparedL1Origin>>,
    },
    /// A verified successor, linked to `current` at construction. Boxed because a
    /// [`LinkedOrigin`] (a full [`Header`](alloy_consensus::Header)) is far larger than the other
    /// variants.
    Ready(Box<LinkedOrigin>),
}

/// Trait for selecting the next L1 origin block for sequencing.
///
/// This trait is used by the sequencer to determine which L1 block should be used
/// as the origin for the next L2 block being built.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait OriginSelector: Debug + Send + Sync {
    /// Selects the next L1 origin block for sequencing.
    ///
    /// # Arguments
    /// * `unsafe_head` - The current unsafe head of the L2 chain
    /// * `is_recovery_mode` - Whether the sequencer is in recovery mode
    ///
    /// # Returns
    /// The selected L1 origin block information, or an error if selection failed.
    async fn next_l1_origin(
        &mut self,
        unsafe_head: L2BlockInfo,
        is_recovery_mode: bool,
    ) -> Result<BlockInfo, L1OriginSelectorError>;
}

/// The [`L1OriginSelector`] is responsible for selecting the L1 origin block based on the
/// current L2 unsafe head's sequence epoch.
///
/// The `current` origin is resolved inline (it is required to build), bounded by
/// [`Self::l1_fetch_timeout`]. The `next` origin is fetched in the background via a self-aborting
/// [`AbortOnDropHandle`] task and adopted only once it links to `current`, so speculating the next
/// epoch never blocks the build loop. The selected origin (header + receipts) is published on a
/// one-slot [`watch`] channel consumed by the attributes builder's
/// [`PrefetchedChainProvider`](super::PrefetchedChainProvider).
#[derive(Debug)]
pub struct L1OriginSelector<P: L1OriginSelectorProvider> {
    /// The [`RollupConfig`].
    cfg: Arc<RollupConfig>,
    /// The [`L1OriginSelectorProvider`], shared with the background next-origin fetch.
    l1: Arc<P>,
    /// The current L1 origin.
    current: Option<PreparedL1Origin>,
    /// The next L1 origin and any in-flight work to prepare it.
    next: NextSlot,
    /// Per-lookup deadline applied to the inline current-origin lookup.
    ///
    /// The current origin is resolved on the sequencer's build critical path, so a single L1
    /// request that blocks for the full transport timeout (see
    /// [`base_consensus_providers::L1_RPC_TIMEOUT`]) would stall block production for that window.
    /// This deadline is deliberately tighter: a slow L1 is treated as "temporarily unavailable" and
    /// retried on the next tick rather than wedging the actor.
    l1_fetch_timeout: Duration,
    /// Per-lookup deadline applied to the background next-origin fetch.
    ///
    /// This runs off the build critical path, so it can be far more lenient than
    /// [`Self::l1_fetch_timeout`]: it only needs to keep a degraded L1 from wedging the background
    /// task, not to protect block production.
    next_fetch_timeout: Duration,
    /// Publishes the selected origin (header + receipts) for the attributes builder.
    origin_tx: watch::Sender<Option<PreparedL1Origin>>,
}

#[async_trait]
impl<P: L1OriginSelectorProvider> OriginSelector for L1OriginSelector<P> {
    /// Determines what the next L1 origin block should be, based off of the [`L2BlockInfo`] unsafe
    /// head.
    ///
    /// The L1 origin is selected based off of the sequencing epoch, determined by the next L2
    /// block's timestamp in relation to the current L1 origin's timestamp. If the next L2
    /// block's timestamp is greater than the L2 unsafe head's L1 origin timestamp, the L1
    /// origin is the block following the current L1 origin.
    async fn next_l1_origin(
        &mut self,
        unsafe_head: L2BlockInfo,
        is_recovery_mode: bool,
    ) -> Result<BlockInfo, L1OriginSelectorError> {
        self.select_origins(&unsafe_head, is_recovery_mode).await?;
        let chosen = self.choose_origin(&unsafe_head)?;

        // Publish the selected origin so the attributes builder can read its header and receipts
        // without a second L1 round-trip.
        self.origin_tx.send_replace(Some(chosen.clone()));
        Ok(chosen.block_info())
    }
}

impl<P: L1OriginSelectorProvider> L1OriginSelector<P> {
    /// Default per-lookup deadline for the inline current-origin lookup.
    ///
    /// Chosen to sit well below the L1 transport timeout so the selector fails fast and reuses the
    /// current origin (its documented best-effort fallback) instead of blocking the build loop for
    /// the full transport window on a degraded L1 endpoint.
    pub const DEFAULT_L1_FETCH_TIMEOUT: Duration = Duration::from_secs(2);

    /// Default per-lookup deadline for the background next-origin fetch. More lenient than
    /// [`Self::DEFAULT_L1_FETCH_TIMEOUT`] because it runs off the build critical path.
    pub const DEFAULT_NEXT_FETCH_TIMEOUT: Duration = Duration::from_secs(12);

    /// Creates a new [`L1OriginSelector`] with the [`Self::DEFAULT_L1_FETCH_TIMEOUT`].
    pub fn new(cfg: Arc<RollupConfig>, l1: P) -> Self {
        Self::with_l1_fetch_timeout(cfg, l1, Self::DEFAULT_L1_FETCH_TIMEOUT)
    }

    /// Creates a new [`L1OriginSelector`] with an explicit inline current-origin fetch deadline.
    pub fn with_l1_fetch_timeout(
        cfg: Arc<RollupConfig>,
        l1: P,
        l1_fetch_timeout: Duration,
    ) -> Self {
        let (origin_tx, _origin_rx) = watch::channel(None);
        Self {
            cfg,
            l1: Arc::new(l1),
            current: None,
            next: NextSlot::Idle,
            l1_fetch_timeout,
            next_fetch_timeout: Self::DEFAULT_NEXT_FETCH_TIMEOUT,
            origin_tx,
        }
    }

    /// Returns a receiver for the selected-origin channel, consumed by the attributes builder's
    /// [`PrefetchedChainProvider`](super::PrefetchedChainProvider).
    pub fn subscribe(&self) -> watch::Receiver<Option<PreparedL1Origin>> {
        self.origin_tx.subscribe()
    }

    /// Returns the current L1 origin.
    pub fn current(&self) -> Option<BlockInfo> {
        self.current.as_ref().map(PreparedL1Origin::block_info)
    }

    /// Returns the next L1 origin, if one has been prepared.
    pub fn next(&self) -> Option<BlockInfo> {
        self.next_ready().map(PreparedL1Origin::block_info)
    }

    /// Returns the prepared `next` origin if the slot currently holds a ready successor.
    const fn next_ready(&self) -> Option<&PreparedL1Origin> {
        match &self.next {
            NextSlot::Ready(linked) => Some(linked.get()),
            _ => None,
        }
    }

    /// Selects the current L1 origin block based on the unsafe head, and drives the background
    /// preparation of the next origin.
    async fn select_origins(
        &mut self,
        unsafe_head: &L2BlockInfo,
        in_recovery_mode: bool,
    ) -> Result<(), L1OriginSelectorError> {
        let origin_hash = unsafe_head.l1_origin.hash;

        if in_recovery_mode {
            // Recovery re-resolves the current origin from scratch; drop any speculative next.
            self.current = self.resolve_current(origin_hash).await?;
            self.next = NextSlot::Idle;
        } else if self.current.as_ref().is_some_and(|c| c.hash == origin_hash) {
            // Do nothing; the next L2 block exists in the same epoch as the current L1 origin.
        } else if let Some(promoted) = self.take_ready_if(origin_hash) {
            // Advance the origin: the unsafe head now sits on our prepared `next`.
            self.current = Some(promoted);
        } else {
            // Cold start, a multi-epoch jump, or an L1 reorg: resolve the current origin inline.
            self.current = self.resolve_current(origin_hash).await?;
            self.next = NextSlot::Idle;
        }

        // Drive the next-origin state machine off the critical path.
        if let Some(current) = self.current.clone() {
            self.poll_next(&current).await;
        }
        Ok(())
    }

    /// Selects the origin to build on from the resolved `current`/`next` state, mirroring the
    /// sequencing-epoch rules. Does not perform I/O.
    fn choose_origin(
        &self,
        unsafe_head: &L2BlockInfo,
    ) -> Result<PreparedL1Origin, L1OriginSelectorError> {
        let next_l2_timestamp =
            self.cfg.l2_block_timestamp(unsafe_head.block_info.number.saturating_add(1));
        let next = self.next_ready();

        // Start building on the next L1 origin block if the next L2 block's timestamp is
        // greater than or equal to the next L1 origin's timestamp.
        if let Some(next) = next
            && next_l2_timestamp >= next.header.timestamp
        {
            return Ok(next.clone());
        }

        let Some(current) = self.current.as_ref() else {
            return Err(L1OriginSelectorError::OriginNotFound(unsafe_head.l1_origin.hash));
        };

        let max_seq_drift = self.cfg.max_sequencer_drift(current.header.timestamp);
        let past_seq_drift =
            next_l2_timestamp.saturating_sub(current.header.timestamp) > max_seq_drift;

        // If the sequencer drift has not been exceeded, return the current L1 origin.
        if !past_seq_drift {
            return Ok(current.clone());
        }

        warn!(
            target: "l1_origin_selector",
            current_origin_time = current.header.timestamp,
            unsafe_head_time = unsafe_head.block_info.timestamp,
            next_l2_time = next_l2_timestamp,
            max_seq_drift,
            "Next L2 block time is past the sequencer drift"
        );

        if next.map(|n| next_l2_timestamp < n.header.timestamp).unwrap_or(false) {
            // If the next L1 origin is ahead of the next L2 block's timestamp, return the current
            // origin.
            return Ok(current.clone());
        }

        next.cloned().ok_or_else(|| L1OriginSelectorError::NotEnoughData(current.block_info()))
    }

    /// Takes the prepared `next` origin if it matches `hash`, resetting the slot to
    /// [`NextSlot::Idle`].
    fn take_ready_if(&mut self, hash: B256) -> Option<PreparedL1Origin> {
        if matches!(&self.next, NextSlot::Ready(linked) if linked.get().hash == hash) {
            match std::mem::take(&mut self.next) {
                NextSlot::Ready(linked) => Some(linked.into_current()),
                _ => None,
            }
        } else {
            None
        }
    }

    /// Resolves the current L1 origin by hash, bounding the request with [`Self::l1_fetch_timeout`].
    ///
    /// A timed-out lookup surfaces as [`L1OriginSelectorError::Timeout`], which the build path
    /// treats as a temporary error and retries on the next tick rather than blocking the loop.
    async fn resolve_current(
        &self,
        hash: B256,
    ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError> {
        timeout(self.l1_fetch_timeout, self.l1.prepared_by_hash(hash)).await.unwrap_or_else(|_| {
            warn!(
                target: "l1_origin_selector",
                hash = %hash,
                timeout_ms = self.l1_fetch_timeout.as_millis(),
                "Timed out fetching current L1 origin by hash; retrying on next tick"
            );
            Metrics::sequencer_l1_origin_fetch_timeouts_total("by_hash").increment(1);
            Err(L1OriginSelectorError::Timeout)
        })
    }

    /// Advances the next-origin state machine without ever awaiting an unfinished fetch.
    ///
    /// A finished fetch is adopted only if its result still links to the live `current` (the
    /// [`LinkedOrigin`] type enforces the check), so a mid-flight reorg cannot promote a stale
    /// successor. An in-flight fetch keyed to a superseded parent is dropped (aborting the task) and
    /// respawned.
    async fn poll_next(&mut self, current: &PreparedL1Origin) {
        self.next = match std::mem::take(&mut self.next) {
            NextSlot::Ready(linked) => NextSlot::Ready(linked),
            NextSlot::InFlight { parent_hash, .. } if parent_hash != current.hash => {
                // `current` advanced under us; the handle drops here (aborting the task). Respawn.
                self.spawn_next(current)
            }
            NextSlot::InFlight { handle, .. } if handle.is_finished() => {
                // Adopt only if the result still links to the live `current`; otherwise (not
                // produced yet, a reorg, or a parent mismatch) retry on the next tick. The handle is
                // finished, so awaiting it resolves immediately.
                handle
                    .await
                    .ok()
                    .flatten()
                    .and_then(|p| LinkedOrigin::link(current, p))
                    .map_or(NextSlot::Idle, |linked| NextSlot::Ready(Box::new(linked)))
            }
            // Still running, off the critical path: leave it in flight.
            in_flight @ NextSlot::InFlight { .. } => in_flight,
            NextSlot::Idle => self.spawn_next(current),
        };
    }

    /// Spawns a background fetch of the origin following `current`, bounded by
    /// [`Self::next_fetch_timeout`]. The task resolves to the prepared next origin, or `None` if it
    /// is not yet available, errored, or timed out.
    fn spawn_next(&self, current: &PreparedL1Origin) -> NextSlot {
        let l1 = Arc::clone(&self.l1);
        let number = current.header.number.saturating_add(1);
        let parent_hash = current.hash;
        let fetch_timeout = self.next_fetch_timeout;
        let handle = AbortOnDropHandle::new(tokio::spawn(async move {
            match timeout(fetch_timeout, l1.prepared_by_number(number)).await {
                Ok(Ok(prepared)) => prepared,
                Ok(Err(_)) => None,
                Err(_) => {
                    Metrics::sequencer_l1_origin_fetch_timeouts_total("by_number").increment(1);
                    None
                }
            }
        }));
        NextSlot::InFlight { parent_hash, handle }
    }

    /// Test-only: deterministically awaits the in-flight next fetch and adopts its result, so tests
    /// can observe the prepared `next` without racing the background task.
    #[cfg(test)]
    async fn settle_next(&mut self) {
        let Some(current) = self.current.clone() else {
            return;
        };
        if let NextSlot::InFlight { handle, .. } = std::mem::take(&mut self.next) {
            self.next = handle
                .await
                .ok()
                .flatten()
                .and_then(|p| LinkedOrigin::link(&current, p))
                .map_or(NextSlot::Idle, |linked| NextSlot::Ready(Box::new(linked)));
        }
    }
}

/// An error produced by the [`L1OriginSelector`].
#[derive(Debug, thiserror::Error)]
pub enum L1OriginSelectorError {
    /// An error produced by the [`RootProvider`](alloy_provider::RootProvider).
    #[error(transparent)]
    Provider(#[from] RpcError<TransportErrorKind>),
    /// The L1 provider does not have enough data to select the next L1 origin block.
    #[error(
        "Waiting for more L1 data to be available to select the next L1 origin block. Current L1 origin: {0:?}"
    )]
    NotEnoughData(BlockInfo),
    /// The L1 origin block was not found by its hash, e.g. during an L1 reorg or sync lag.
    #[error("L1 origin block not found by hash: {0}")]
    OriginNotFound(B256),
    /// An L1 lookup exceeded the origin selector's per-request fetch deadline. Treated as a
    /// temporary error so the build path retries on the next tick instead of blocking the loop.
    #[error("timed out fetching L1 origin block")]
    Timeout,
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use alloy_consensus::Header;
    use alloy_eips::NumHash;
    use rstest::rstest;

    use super::*;

    /// A mock [`L1OriginSelectorProvider`] with a local set of [`BlockInfo`]s available.
    #[derive(Default, Debug, Clone)]
    struct MockOriginSelectorProvider {
        blocks: HashSet<BlockInfo>,
        failed_number_fetches: HashSet<u64>,
        /// Artificial latency applied to every lookup, used to exercise the fetch timeout.
        fetch_delay: Duration,
    }

    impl MockOriginSelectorProvider {
        /// Adds a block to the set of available blocks.
        fn with_block(&mut self, block: BlockInfo) {
            self.blocks.insert(block);
        }

        /// Fails lookups for the given L1 block number.
        fn fail_block_number(&mut self, number: u64) {
            self.failed_number_fetches.insert(number);
        }

        /// Applies an artificial delay to every lookup.
        fn with_fetch_delay(&mut self, delay: Duration) {
            self.fetch_delay = delay;
        }

        /// Synthesizes prepared origin state from a [`BlockInfo`], with empty receipts.
        fn prepared(block: &BlockInfo) -> PreparedL1Origin {
            PreparedL1Origin {
                hash: block.hash,
                header: Header {
                    number: block.number,
                    timestamp: block.timestamp,
                    parent_hash: block.parent_hash,
                    ..Default::default()
                },
                receipts: Arc::new(vec![]),
            }
        }
    }

    #[async_trait]
    impl L1OriginSelectorProvider for MockOriginSelectorProvider {
        async fn prepared_by_hash(
            &self,
            hash: B256,
        ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError> {
            tokio::time::sleep(self.fetch_delay).await;
            Ok(self.blocks.iter().find(|b| b.hash == hash).map(Self::prepared))
        }

        async fn prepared_by_number(
            &self,
            number: u64,
        ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError> {
            tokio::time::sleep(self.fetch_delay).await;
            if self.failed_number_fetches.contains(&number) {
                return Err(L1OriginSelectorError::Provider(TransportErrorKind::custom_str(
                    "mock L1 block fetch failed",
                )));
            }
            Ok(self.blocks.iter().find(|b| b.number == number).map(Self::prepared))
        }
    }

    #[tokio::test]
    #[rstest]
    #[case::single_epoch(1)]
    #[case::many_epochs(12)]
    async fn test_next_l1_origin_several_epochs(#[case] num_epochs: usize) {
        // Assume an L1 slot time of 12 seconds.
        const L1_SLOT_TIME: u64 = 12;
        // Assume an L2 block time of 2 seconds.
        const L2_BLOCK_TIME: u64 = 2;

        // Initialize the rollup configuration with a block time of 2 seconds and a sequencer drift
        // of 600 seconds.
        let cfg = Arc::new(RollupConfig {
            block_time: L2_BLOCK_TIME,
            max_sequencer_drift: 600,
            ..Default::default()
        });

        // Initialize the provider with mock L1 blocks, equal to the number of epochs + 1
        // (such that the next logical origin is always available.)
        let mut provider = MockOriginSelectorProvider::default();
        for i in 0..num_epochs + 1 {
            provider.with_block(BlockInfo {
                parent_hash: B256::with_last_byte(i.saturating_sub(1) as u8),
                hash: B256::with_last_byte(i as u8),
                number: i as u64,
                timestamp: i as u64 * L1_SLOT_TIME,
            });
        }

        let mut selector = L1OriginSelector::new(Arc::clone(&cfg), provider);

        // Ensure all L1 origin blocks are produced correctly for each L2 block within all available
        // epochs.
        for i in 0..(num_epochs as u64 * (L1_SLOT_TIME / cfg.block_time)) {
            let current_epoch = (i * cfg.block_time) / L1_SLOT_TIME;
            let unsafe_head = L2BlockInfo {
                block_info: BlockInfo {
                    hash: B256::ZERO,
                    number: i,
                    timestamp: i * cfg.block_time,
                    ..Default::default()
                },
                l1_origin: NumHash {
                    number: current_epoch,
                    hash: B256::with_last_byte(current_epoch as u8),
                },
                seq_num: 0,
            };
            let next = selector.next_l1_origin(unsafe_head, false).await.unwrap();
            // Deterministically settle the background next-origin fetch so it is ready to be
            // selected on the epoch-crossing tick.
            selector.settle_next().await;

            // The expected L1 origin block is the one corresponding to the epoch of the current L2
            // block.
            let expected_epoch = ((i + 1) * cfg.block_time) / L1_SLOT_TIME;
            assert_eq!(next.hash, B256::with_last_byte(expected_epoch as u8));
            assert_eq!(next.number, expected_epoch);
        }
    }

    /// Tests that [`L1OriginSelectorError::OriginNotFound`] is returned (rather than a panic)
    /// when the L1 provider cannot find the current origin block by hash, e.g. after a reorg.
    #[tokio::test]
    async fn test_next_l1_origin_not_found() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });

        // Provider has no blocks, simulating a block disappearing due to an L1 reorg.
        let provider = MockOriginSelectorProvider::default();
        let mut selector = L1OriginSelector::new(Arc::clone(&cfg), provider);

        let unsafe_head = L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::with_last_byte(1),
                number: 1,
                timestamp: 2,
                ..Default::default()
            },
            l1_origin: NumHash { number: 0, hash: B256::with_last_byte(42) },
            seq_num: 0,
        };

        let err = selector.next_l1_origin(unsafe_head, false).await.unwrap_err();
        assert!(
            matches!(err, L1OriginSelectorError::OriginNotFound(h) if h == B256::with_last_byte(42))
        );
    }

    #[tokio::test]
    #[rstest]
    #[case::not_available(false)]
    #[case::is_available(true)]
    async fn test_next_l1_origin_next_maybe_available(#[case] next_l1_origin_available: bool) {
        // Assume an L2 block time of 2 seconds.
        const L2_BLOCK_TIME: u64 = 2;

        // Initialize the rollup configuration with a block time of 2 seconds and a sequencer drift
        // of 600 seconds.
        let cfg = Arc::new(RollupConfig {
            block_time: L2_BLOCK_TIME,
            max_sequencer_drift: 600,
            ..Default::default()
        });

        // Initialize the provider with a single L1 block.
        let mut provider = MockOriginSelectorProvider::default();
        provider.with_block(BlockInfo {
            parent_hash: B256::ZERO,
            hash: B256::ZERO,
            number: 0,
            timestamp: 0,
        });

        if next_l1_origin_available {
            // If the next L1 origin is available, add it to the provider.
            provider.with_block(BlockInfo {
                parent_hash: B256::ZERO,
                hash: B256::with_last_byte(1),
                number: 1,
                timestamp: cfg.block_time,
            });
        }

        let mut selector = L1OriginSelector::new(Arc::clone(&cfg), provider);

        let current_epoch = 0;
        let unsafe_head = L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::ZERO,
                number: 5,
                timestamp: 5 * cfg.block_time,
                ..Default::default()
            },
            l1_origin: NumHash {
                number: current_epoch,
                hash: B256::with_last_byte(current_epoch as u8),
            },
            seq_num: 0,
        };
        // The first call resolves `current` and kicks off the background next-origin fetch; settle
        // it so the second call observes the prepared `next`.
        selector.next_l1_origin(unsafe_head, false).await.unwrap();
        selector.settle_next().await;
        let next = selector.next_l1_origin(unsafe_head, false).await.unwrap();

        // The expected L1 origin block is the one corresponding to the epoch of the current L2
        // block. Assuming the next L1 origin block is not available from the eyes of the
        // provider (_and_ it is not past the sequencer drift), the current L1 origin block
        // will be re-used.
        let expected_epoch =
            if next_l1_origin_available { current_epoch + 1 } else { current_epoch };
        assert_eq!(next.hash, B256::with_last_byte(expected_epoch as u8));
        assert_eq!(next.number, expected_epoch);
    }

    #[tokio::test]
    async fn test_next_l1_origin_reuses_current_when_next_fetch_fails_before_seq_drift() {
        const L2_BLOCK_TIME: u64 = 2;
        const MAX_SEQUENCER_DRIFT: u64 = 30 * 60;

        let cfg = Arc::new(RollupConfig {
            block_time: L2_BLOCK_TIME,
            max_sequencer_drift: MAX_SEQUENCER_DRIFT,
            ..Default::default()
        });

        let current =
            BlockInfo { parent_hash: B256::ZERO, hash: B256::ZERO, number: 0, timestamp: 0 };
        let mut provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.fail_block_number(current.number + 1);

        let mut selector = L1OriginSelector::new(Arc::clone(&cfg), provider);
        let unsafe_head = L2BlockInfo {
            block_info: BlockInfo {
                number: (MAX_SEQUENCER_DRIFT - cfg.block_time) / cfg.block_time,
                timestamp: MAX_SEQUENCER_DRIFT - cfg.block_time,
                ..Default::default()
            },
            l1_origin: NumHash { number: current.number, hash: current.hash },
            seq_num: 0,
        };

        let next = selector.next_l1_origin(unsafe_head, false).await.unwrap();
        selector.settle_next().await;

        assert_eq!(next, current);
        assert_eq!(selector.current(), Some(current));
        assert_eq!(selector.next(), None);
    }

    #[tokio::test]
    async fn test_recovery_mode_reuses_current_on_next_fetch_error_before_seq_drift() {
        const L2_BLOCK_TIME: u64 = 2;
        const MAX_SEQUENCER_DRIFT: u64 = 30 * 60;

        let cfg = Arc::new(RollupConfig {
            block_time: L2_BLOCK_TIME,
            max_sequencer_drift: MAX_SEQUENCER_DRIFT,
            ..Default::default()
        });

        let current =
            BlockInfo { parent_hash: B256::ZERO, hash: B256::ZERO, number: 0, timestamp: 0 };
        let mut provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.fail_block_number(current.number + 1);

        let mut selector = L1OriginSelector::new(Arc::clone(&cfg), provider);
        let unsafe_head = L2BlockInfo {
            block_info: BlockInfo {
                number: (MAX_SEQUENCER_DRIFT - cfg.block_time) / cfg.block_time,
                timestamp: MAX_SEQUENCER_DRIFT - cfg.block_time,
                ..Default::default()
            },
            l1_origin: NumHash { number: current.number, hash: current.hash },
            seq_num: 0,
        };

        let next = selector.next_l1_origin(unsafe_head, true).await.unwrap();
        selector.settle_next().await;

        assert_eq!(next, current);
        assert_eq!(selector.current(), Some(current));
        assert_eq!(selector.next(), None);
    }

    #[tokio::test]
    async fn test_next_l1_origin_errors_when_next_fetch_fails_past_seq_drift() {
        const L2_BLOCK_TIME: u64 = 2;
        const MAX_SEQUENCER_DRIFT: u64 = 30 * 60;

        let cfg = Arc::new(RollupConfig {
            block_time: L2_BLOCK_TIME,
            max_sequencer_drift: MAX_SEQUENCER_DRIFT,
            ..Default::default()
        });

        let current =
            BlockInfo { parent_hash: B256::ZERO, hash: B256::ZERO, number: 0, timestamp: 0 };
        let mut provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.fail_block_number(current.number + 1);

        let mut selector = L1OriginSelector::new(Arc::clone(&cfg), provider);
        let unsafe_head = L2BlockInfo {
            block_info: BlockInfo {
                number: MAX_SEQUENCER_DRIFT / cfg.block_time,
                timestamp: MAX_SEQUENCER_DRIFT,
                ..Default::default()
            },
            l1_origin: NumHash { number: current.number, hash: current.hash },
            seq_num: 0,
        };

        // The first call kicks off the (failing) next-origin fetch; settle it, then the second call
        // observes that no next origin is available past the drift.
        selector.next_l1_origin(unsafe_head, false).await.ok();
        selector.settle_next().await;
        let err = selector.next_l1_origin(unsafe_head, false).await.unwrap_err();

        assert!(matches!(err, L1OriginSelectorError::NotEnoughData(block) if block == current));
        assert_eq!(selector.current(), Some(current));
        assert_eq!(selector.next(), None);
    }

    #[tokio::test]
    #[rstest]
    #[case::next_not_available(false, false)]
    #[case::next_available_but_behind(true, false)]
    #[case::next_available_and_ahead(true, true)]
    async fn test_next_l1_origin_next_past_seq_drift(
        #[case] next_available: bool,
        #[case] next_ahead_of_unsafe: bool,
    ) {
        // Assume an L2 block time of 2 seconds.
        const L2_BLOCK_TIME: u64 = 2;

        // Initialize the rollup configuration with a block time of 2 seconds and a sequencer drift
        // of 600 seconds.
        let cfg = Arc::new(RollupConfig {
            block_time: L2_BLOCK_TIME,
            max_sequencer_drift: 600,
            ..Default::default()
        });

        // Initialize the provider with a single L1 block.
        let mut provider = MockOriginSelectorProvider::default();
        provider.with_block(BlockInfo {
            parent_hash: B256::ZERO,
            hash: B256::ZERO,
            number: 0,
            timestamp: 0,
        });

        if next_available {
            // If the next L1 origin is to be available, add it to the provider.
            provider.with_block(BlockInfo {
                parent_hash: B256::ZERO,
                hash: B256::with_last_byte(1),
                number: 1,
                timestamp: if next_ahead_of_unsafe {
                    cfg.max_sequencer_drift + cfg.block_time * 2
                } else {
                    cfg.block_time
                },
            });
        }

        let mut selector = L1OriginSelector::new(Arc::clone(&cfg), provider);

        let current_epoch = 0;
        let unsafe_head = L2BlockInfo {
            block_info: BlockInfo {
                number: cfg.max_sequencer_drift / cfg.block_time,
                timestamp: cfg.max_sequencer_drift,
                ..Default::default()
            },
            l1_origin: NumHash {
                number: current_epoch,
                hash: B256::with_last_byte(current_epoch as u8),
            },
            seq_num: 0,
        };

        // Kick off and settle the background next-origin fetch, then observe the selection. The
        // first call may error (past drift, next not yet prepared); that is expected.
        selector.next_l1_origin(unsafe_head, false).await.ok();
        selector.settle_next().await;

        if next_available {
            let next = selector.next_l1_origin(unsafe_head, false).await.unwrap();
            if next_ahead_of_unsafe {
                // If the next L1 origin is available and ahead of the unsafe head, the L1 origin
                // should not change.
                assert_eq!(next.hash, B256::ZERO);
                assert_eq!(next.number, 0);
            } else {
                // If the next L1 origin is available and behind the unsafe head, the L1 origin
                // should advance.
                assert_eq!(next.hash, B256::with_last_byte(1));
                assert_eq!(next.number, 1);
            }
        } else {
            // If we're past the sequencer drift, and the next L1 block is not available, a
            // `NotEnoughData` error should be returned signifying that we cannot
            // proceed with the next L1 origin until the block is present.
            let next_err = selector.next_l1_origin(unsafe_head, false).await.unwrap_err();
            assert!(matches!(next_err, L1OriginSelectorError::NotEnoughData(_)));
        }
    }

    /// The background next-origin fetch is bounded: when the by-number lookup is slower than the
    /// fetch deadline, the fetch times out and no next origin is prepared, so the selector reuses
    /// the current origin instead of blocking the build loop.
    #[tokio::test(start_paused = true)]
    async fn test_next_origin_fetch_timeout_reuses_current() {
        const L2_BLOCK_TIME: u64 = 2;
        const MAX_SEQUENCER_DRIFT: u64 = 30 * 60;

        let cfg = Arc::new(RollupConfig {
            block_time: L2_BLOCK_TIME,
            max_sequencer_drift: MAX_SEQUENCER_DRIFT,
            ..Default::default()
        });

        let current =
            BlockInfo { parent_hash: B256::ZERO, hash: B256::ZERO, number: 0, timestamp: 0 };
        let mut provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        // The next origin exists, but the lookup is slower than the fetch deadline.
        provider.with_block(BlockInfo {
            parent_hash: current.hash,
            hash: B256::with_last_byte(1),
            number: 1,
            timestamp: L2_BLOCK_TIME,
        });
        provider.with_fetch_delay(Duration::from_secs(30));

        let mut selector = L1OriginSelector::new(Arc::clone(&cfg), provider);
        // Seed the current origin so `select_origins` takes the steady-state branch and only the
        // background by-number lookup is exercised.
        selector.current = Some(MockOriginSelectorProvider::prepared(&current));

        let unsafe_head = L2BlockInfo {
            block_info: BlockInfo { number: 1, timestamp: L2_BLOCK_TIME, ..Default::default() },
            l1_origin: NumHash { number: current.number, hash: current.hash },
            seq_num: 0,
        };

        let next = selector.next_l1_origin(unsafe_head, false).await.unwrap();
        // Settling awaits the in-flight fetch, whose internal deadline (12s) fires before the 30s
        // mock delay, so no next origin is adopted.
        selector.settle_next().await;

        assert_eq!(next, current);
        assert_eq!(selector.next(), None);
    }

    /// The load-bearing by-hash lookup is bounded too: a slow current-origin lookup surfaces as a
    /// [`L1OriginSelectorError::Timeout`] (a temporary error retried on the next tick) rather than
    /// wedging the sequencer for the full transport window.
    #[tokio::test(start_paused = true)]
    async fn test_current_origin_fetch_timeout_errors() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });

        let current = BlockInfo {
            parent_hash: B256::ZERO,
            hash: B256::with_last_byte(7),
            number: 3,
            timestamp: 6,
        };
        let mut provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.with_fetch_delay(Duration::from_secs(30));

        let mut selector = L1OriginSelector::with_l1_fetch_timeout(
            Arc::clone(&cfg),
            provider,
            Duration::from_millis(500),
        );

        // `current` is unset, so `select_origins` must look up the origin by hash, which times out.
        let unsafe_head = L2BlockInfo {
            block_info: BlockInfo { number: 3, timestamp: 6, ..Default::default() },
            l1_origin: NumHash { number: current.number, hash: current.hash },
            seq_num: 0,
        };

        let err = selector.next_l1_origin(unsafe_head, false).await.unwrap_err();

        assert!(matches!(err, L1OriginSelectorError::Timeout));
        assert_eq!(selector.current(), None);
    }
}
