//! The [`L1OriginSelector`] and its next-origin state machine.

use std::{fmt::Debug, sync::Arc};

use alloy_primitives::B256;
use alloy_transport::{RpcError, TransportErrorKind};
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_protocol::{BlockInfo, L2BlockInfo};
use tokio::sync::watch;
use tokio_util::task::AbortOnDropHandle;

use super::{L1OriginSelectorProvider, PreparedL1Origin};
use crate::Metrics;

/// The speculative successor to `current` and any in-flight work to obtain it.
#[derive(Debug, Default)]
enum NextSlot {
    /// No successor is known and no fetch is running.
    #[default]
    Idle,
    /// A background fetch keyed to the current origin and observed L1 chain view.
    InFlight {
        /// The current origin hash this fetch must extend.
        parent_hash: B256,
        /// The observed L1 head hash under which the fetch started.
        chain_view: B256,
        /// The self-aborting background fetch.
        handle: AbortOnDropHandle<Option<PreparedL1Origin>>,
    },
    /// A successor verified against its parent and observed L1 chain view.
    Ready {
        /// The prepared next origin.
        block: Box<PreparedL1Origin>,
        /// The observed L1 head hash under which the origin was fetched.
        chain_view: B256,
    },
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
    ///
    /// # Returns
    /// The selected L1 origin block information, or an error if selection failed.
    async fn next_l1_origin(
        &mut self,
        unsafe_head: L2BlockInfo,
    ) -> Result<BlockInfo, L1OriginSelectorError>;
}

/// The [`L1OriginSelector`] is responsible for selecting the L1 origin block based on the
/// current L2 unsafe head's sequence epoch.
///
/// Next-origin lookups run in a self-aborting background task and are adopted only if they still
/// extend the current origin under the same observed L1 chain view. While sequencer drift permits,
/// an unfinished lookup does not prevent building on the current origin. Once drift is exceeded,
/// selection returns [`L1OriginSelectorError::NotEnoughData`] until the lookup is ready rather than
/// awaiting speculative work on the build path. Before the first observed L1 head, the selector may
/// reuse the exact-hash current origin accepted by the engine reset, but it cannot prepare or advance
/// to a successor. An L1 reorg that orphans the current origin is therefore detected when the
/// background successor lookup completes its parent check, after which the sequencer requests an
/// engine reset to rewind any affected unsafe L2 blocks.
#[derive(Debug)]
pub struct L1OriginSelector<P: L1OriginSelectorProvider> {
    /// The [`RollupConfig`].
    cfg: Arc<RollupConfig>,
    /// The [`L1OriginSelectorProvider`], shared with the background fetch.
    l1: Arc<P>,
    /// The current L1 origin.
    current: Option<PreparedL1Origin>,
    /// The next L1 origin and any in-flight work to obtain it.
    next: NextSlot,
    /// Publishes the prepared origin selected for the next payload.
    selected_tx: watch::Sender<Option<PreparedL1Origin>>,
}

#[async_trait]
impl<P: L1OriginSelectorProvider + Send + Sync> OriginSelector for L1OriginSelector<P> {
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
    ) -> Result<BlockInfo, L1OriginSelectorError> {
        if let Err(error) = self.select_origins(&unsafe_head).await {
            self.selected_tx.send_replace(None);
            return Err(error);
        }
        let selected = match self.choose_origin(&unsafe_head) {
            Ok(selected) => selected,
            Err(error) => {
                self.selected_tx.send_replace(None);
                return Err(error);
            }
        };
        let block_info = selected.block_info();
        self.selected_tx.send_replace(Some(selected));
        Ok(block_info)
    }
}

impl<P: L1OriginSelectorProvider> L1OriginSelector<P> {
    /// Creates a new [`L1OriginSelector`].
    pub fn new(cfg: Arc<RollupConfig>, l1: P) -> Self {
        Self {
            cfg,
            l1: Arc::new(l1),
            current: None,
            next: NextSlot::Idle,
            selected_tx: watch::Sender::new(None),
        }
    }

    /// Subscribes to the prepared origin selected for payload building.
    pub fn subscribe(&self) -> watch::Receiver<Option<PreparedL1Origin>> {
        self.selected_tx.subscribe()
    }

    /// Returns the current L1 origin.
    pub fn current(&self) -> Option<BlockInfo> {
        self.current.as_ref().map(PreparedL1Origin::block_info)
    }

    /// Returns the next L1 origin if its background fetch has completed and been adopted.
    pub fn next(&self) -> Option<BlockInfo> {
        self.next_ready().map(PreparedL1Origin::block_info)
    }

    /// Returns the ready successor, if any.
    fn next_ready(&self) -> Option<&PreparedL1Origin> {
        match &self.next {
            NextSlot::Ready { block, .. } => Some(block.as_ref()),
            _ => None,
        }
    }

    /// Selects the origin to build on from the current selector state without performing I/O.
    fn choose_origin(
        &self,
        unsafe_head: &L2BlockInfo,
    ) -> Result<PreparedL1Origin, L1OriginSelectorError> {
        let next_l2_timestamp =
            self.cfg.l2_block_timestamp(unsafe_head.block_info.number.saturating_add(1));
        let Some(current) = self.current.as_ref() else {
            return Err(L1OriginSelectorError::OriginNotFound(unsafe_head.l1_origin.hash));
        };
        let next = self.next_ready();

        // Start building on the next L1 origin block if the next L2 block's timestamp is
        // greater than or equal to the next L1 origin's timestamp.
        if let Some(next) = next
            && next_l2_timestamp >= next.header.timestamp
        {
            return Ok(next.clone());
        }

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

        if next.map(|next| next_l2_timestamp < next.header.timestamp).unwrap_or(false) {
            // If the next L1 origin is ahead of the next L2 block's timestamp, return the current
            // origin.
            return Ok(current.clone());
        }

        // Any ready successor was already handled above: a successor at or behind the next L2
        // timestamp returns at the first check, and one ahead of it returns the current origin.
        // An in-flight background fetch is intentionally not exposed as `next`, so past the drift
        // limit the caller must defer the build until the verified successor becomes ready rather
        // than continue building on an expired origin.
        debug_assert!(next.is_none(), "a ready successor must be handled before the drift path");
        Err(L1OriginSelectorError::NotEnoughData(current.block_info()))
    }

    /// Selects the current origin and drives the background next-origin state machine.
    async fn select_origins(
        &mut self,
        unsafe_head: &L2BlockInfo,
    ) -> Result<(), L1OriginSelectorError> {
        let origin_hash = unsafe_head.l1_origin.hash;

        if self.current.as_ref().is_some_and(|current| current.hash == origin_hash) {
            // The next L2 block remains in the current sequencing epoch.
        } else if let Some(promoted) = self.take_ready_if(origin_hash) {
            // The accepted unsafe head confirms that the prepared successor was used. Promote it
            // before applying speculative chain-view invalidation so a normal L1 head advance
            // cannot discard the exact origin required by the accepted L2 parent.
            self.current = Some(promoted);
        } else {
            // Cold start, multi-epoch jump, or reorg: resolve the required current origin.
            self.next = NextSlot::Idle;
            self.current = self.l1.prepared_by_hash(origin_hash).await?;
        }

        if let Some(current) = self.current.as_ref() {
            self.poll_next(current.hash, current.header.number).await?;
        }
        Ok(())
    }

    /// Takes a ready successor if it matches `hash`.
    fn take_ready_if(&mut self, hash: B256) -> Option<PreparedL1Origin> {
        if matches!(&self.next, NextSlot::Ready { block, .. } if block.hash == hash) {
            match std::mem::take(&mut self.next) {
                NextSlot::Ready { block, .. } => Some(*block),
                _ => None,
            }
        } else {
            None
        }
    }

    /// Advances the next-origin state machine without awaiting an unfinished fetch.
    async fn poll_next(
        &mut self,
        current_hash: B256,
        current_number: u64,
    ) -> Result<(), L1OriginSelectorError> {
        let Some(chain_view) = self.l1.chain_view() else {
            self.next = NextSlot::Idle;
            return Ok(());
        };

        // Taking the slot first intentionally leaves it idle if successor adoption proves the
        // current origin was orphaned and returns an error. Stale speculative state must not
        // survive that error path.
        self.next = match std::mem::take(&mut self.next) {
            NextSlot::Ready { block, chain_view: ready_view }
                if block.header.parent_hash == current_hash && ready_view == chain_view =>
            {
                NextSlot::Ready { block, chain_view: ready_view }
            }
            NextSlot::InFlight { parent_hash, chain_view: fetch_view, .. }
                if parent_hash != current_hash || fetch_view != chain_view =>
            {
                self.spawn_next(current_hash, current_number, chain_view)
            }
            NextSlot::InFlight { handle, .. } if handle.is_finished() => {
                let fetched = match handle.await {
                    Ok(fetched) => fetched,
                    Err(error) => {
                        warn!(
                            target: "l1_origin_selector",
                            error = %error,
                            "Background next-origin task failed; retrying on next tick"
                        );
                        None
                    }
                };
                match self.adopt_next(current_hash, chain_view, fetched)? {
                    NextSlot::Idle => self.l1.chain_view().map_or(NextSlot::Idle, |live_view| {
                        self.spawn_next(current_hash, current_number, live_view)
                    }),
                    adopted => adopted,
                }
            }
            in_flight @ NextSlot::InFlight { .. } => {
                // Preserve the sole outstanding fetch without awaiting it or spawning another.
                // This keeps speculative L1 latency off the block-building path.
                in_flight
            }
            NextSlot::Idle | NextSlot::Ready { .. } => {
                self.spawn_next(current_hash, current_number, chain_view)
            }
        };
        Ok(())
    }

    /// Adopts a fetched successor only if its parent and observed chain view are still current.
    fn adopt_next(
        &self,
        current_hash: B256,
        chain_view: B256,
        fetched: Option<PreparedL1Origin>,
    ) -> Result<NextSlot, L1OriginSelectorError> {
        let Some(block) = fetched else {
            return Ok(NextSlot::Idle);
        };
        // A result fetched against an obsolete observed head says nothing about the live canonical
        // successor. Discard it before checking ancestry so it cannot be mistaken for a reorg.
        if self.l1.chain_view() != Some(chain_view) {
            return Ok(NextSlot::Idle);
        }
        if block.header.parent_hash != current_hash {
            Metrics::sequencer_l1_origin_orphans_total().increment(1);
            return Err(L1OriginSelectorError::NextL1OriginOrphaned {
                current: current_hash,
                next: block.hash,
            });
        }
        if block.receipts.is_none() {
            return Ok(NextSlot::Idle);
        }
        Ok(NextSlot::Ready { block: Box::new(block), chain_view })
    }

    /// Starts a background lookup for the origin following `current_number`.
    fn spawn_next(&self, current_hash: B256, current_number: u64, chain_view: B256) -> NextSlot {
        let l1 = Arc::clone(&self.l1);
        let number = current_number.saturating_add(1);
        let handle = AbortOnDropHandle::new(tokio::spawn(async move {
            match l1.prepared_by_number(number).await {
                Ok(next) => next,
                Err(error) => {
                    warn!(
                        target: "l1_origin_selector",
                        error = %error,
                        number,
                        "Background next-origin fetch failed; retrying on next tick"
                    );
                    None
                }
            }
        }));
        NextSlot::InFlight { parent_hash: current_hash, chain_view, handle }
    }

    /// Test-only helper that settles an in-flight fetch through the production adoption path.
    #[cfg(test)]
    async fn await_inflight(&mut self) {
        let Some(current_hash) = self.current.as_ref().map(|current| current.hash) else {
            return;
        };
        self.next = match std::mem::take(&mut self.next) {
            NextSlot::InFlight { chain_view, handle, .. } => {
                let fetched = handle.await.ok().flatten();
                self.adopt_next(current_hash, chain_view, fetched)
                    .expect("test fetch must extend the current origin")
            }
            slot => slot,
        };
    }

    /// Waits until an in-flight fetch can be settled through the production polling path.
    #[cfg(test)]
    async fn wait_for_inflight_completion(&self) {
        debug_assert!(
            matches!(&self.next, NextSlot::InFlight { .. }),
            "wait_for_inflight_completion called without an InFlight slot"
        );
        loop {
            if matches!(&self.next, NextSlot::InFlight { handle, .. } if handle.is_finished()) {
                return;
            }
            tokio::task::yield_now().await;
        }
    }
}

/// An error produced by the [`L1OriginSelector`].
#[derive(Debug, thiserror::Error)]
pub enum L1OriginSelectorError {
    /// An error produced by the [`alloy_provider::RootProvider`].
    #[error(transparent)]
    Provider(#[from] RpcError<TransportErrorKind>),
    /// Sequencer drift requires advancing, but no verified next L1 origin is ready yet.
    #[error(
        "Waiting for more L1 data to be available to select the next L1 origin block. Current L1 origin: {0:?}"
    )]
    NotEnoughData(BlockInfo),
    /// The L1 origin block was not found by its hash, e.g. during an L1 reorg or sync lag.
    #[error("L1 origin block not found by hash: {0}")]
    OriginNotFound(B256),
    /// The canonical successor no longer extends the accepted current L1 origin.
    #[error("next L1 origin {next} does not extend current L1 origin {current}")]
    NextL1OriginOrphaned {
        /// Hash of the accepted current L1 origin.
        current: B256,
        /// Hash of its canonical-height successor.
        next: B256,
    },
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Mutex, time::Duration};

    use alloy_eips::NumHash;
    #[cfg(feature = "metrics")]
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use rstest::rstest;
    use tokio::time::timeout;

    use super::*;

    /// A mock [`OriginSelectorProvider`] with a local set of [`BlockInfo`]s available.
    #[derive(Default, Debug, Clone)]
    struct MockOriginSelectorProvider {
        blocks: Arc<Mutex<HashSet<BlockInfo>>>,
        chain_view: Arc<Mutex<Option<B256>>>,
        chain_view_after_number_fetch: Arc<Mutex<Option<B256>>>,
        failed_number_fetches: HashSet<u64>,
        missing_receipts: Arc<Mutex<HashSet<u64>>>,
        number_delay: Arc<Mutex<Duration>>,
    }

    impl MockOriginSelectorProvider {
        /// Creates a new [`MockOriginSelectorProvider`].
        fn with_block(&self, block: BlockInfo) {
            self.blocks.lock().expect("blocks lock poisoned").insert(block);
            *self.chain_view.lock().expect("chain view lock poisoned") = Some(block.hash);
        }

        /// Replaces the block at the same number in the canonical chain view.
        fn replace_block(&self, block: BlockInfo) {
            let mut blocks = self.blocks.lock().expect("blocks lock poisoned");
            blocks.retain(|candidate| candidate.number != block.number);
            blocks.insert(block);
        }

        /// Removes a block from the provider by hash.
        fn remove_block(&self, hash: B256) {
            self.blocks.lock().expect("blocks lock poisoned").retain(|block| block.hash != hash);
        }

        /// Sets the latest observed L1 head hash.
        fn set_chain_view(&self, chain_view: B256) {
            *self.chain_view.lock().expect("chain view lock poisoned") = Some(chain_view);
        }

        /// Clears the latest observed L1 head.
        fn clear_chain_view(&self) {
            *self.chain_view.lock().expect("chain view lock poisoned") = None;
        }

        /// Changes the observed L1 head after the next lookup by number.
        fn change_chain_view_after_number_fetch(&self, chain_view: Option<B256>) {
            *self.chain_view_after_number_fetch.lock().expect("next chain view lock poisoned") =
                chain_view;
        }

        /// Fails lookups for the given L1 block number.
        fn fail_block_number(&mut self, number: u64) {
            self.failed_number_fetches.insert(number);
        }

        /// Omits receipts from lookups for the given L1 block number.
        fn omit_receipts(&self, number: u64) {
            self.missing_receipts.lock().expect("missing receipts lock poisoned").insert(number);
        }

        /// Restores receipts for lookups at the given L1 block number.
        fn restore_receipts(&self, number: u64) {
            self.missing_receipts.lock().expect("missing receipts lock poisoned").remove(&number);
        }

        /// Delays lookups by number.
        fn set_number_delay(&self, delay: Duration) {
            *self.number_delay.lock().expect("number delay lock poisoned") = delay;
        }
    }

    #[async_trait]
    impl L1OriginSelectorProvider for MockOriginSelectorProvider {
        fn chain_view(&self) -> Option<B256> {
            *self.chain_view.lock().expect("chain view lock poisoned")
        }

        async fn prepared_by_hash(
            &self,
            hash: B256,
        ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError> {
            Ok(self
                .blocks
                .lock()
                .expect("blocks lock poisoned")
                .iter()
                .find(|block| block.hash == hash)
                .copied()
                .map(prepared))
        }

        async fn prepared_by_number(
            &self,
            number: u64,
        ) -> Result<Option<PreparedL1Origin>, L1OriginSelectorError> {
            let delay = *self.number_delay.lock().expect("number delay lock poisoned");
            tokio::time::sleep(delay).await;
            if self.failed_number_fetches.contains(&number) {
                return Err(L1OriginSelectorError::Provider(TransportErrorKind::custom_str(
                    "mock L1 block fetch failed",
                )));
            }

            let block = self
                .blocks
                .lock()
                .expect("blocks lock poisoned")
                .iter()
                .find(|block| block.number == number)
                .copied();
            if let Some(chain_view) =
                *self.chain_view_after_number_fetch.lock().expect("next chain view lock poisoned")
            {
                self.set_chain_view(chain_view);
            }
            Ok(block.map(|block| {
                let mut prepared = prepared(block);
                if self
                    .missing_receipts
                    .lock()
                    .expect("missing receipts lock poisoned")
                    .contains(&number)
                {
                    prepared.receipts = None;
                }
                prepared
            }))
        }
    }

    fn prepared(block: BlockInfo) -> PreparedL1Origin {
        PreparedL1Origin {
            hash: block.hash,
            header: alloy_consensus::Header {
                parent_hash: block.parent_hash,
                number: block.number,
                timestamp: block.timestamp,
                ..Default::default()
            },
            receipts: Some(Arc::new(Vec::new())),
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
        let provider = MockOriginSelectorProvider::default();
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
            let _ = selector.next_l1_origin(unsafe_head).await;
            selector.await_inflight().await;
            assert!(selector.next().is_some(), "next origin not ready at L2 block {i}");
            let next = selector.next_l1_origin(unsafe_head).await.unwrap();

            // The expected L1 origin block is the one corresponding to the epoch of the current L2
            // block.
            let expected_epoch = ((i + 1) * cfg.block_time) / L1_SLOT_TIME;
            assert_eq!(
                next.hash,
                B256::with_last_byte(expected_epoch as u8),
                "unexpected origin at L2 block {i}"
            );
            assert_eq!(next.number, expected_epoch, "unexpected origin at L2 block {i}");
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

        let err = selector.next_l1_origin(unsafe_head).await.unwrap_err();
        assert!(
            matches!(err, L1OriginSelectorError::OriginNotFound(h) if h == B256::with_last_byte(42))
        );
    }

    #[tokio::test]
    async fn test_next_l1_origin_refreshes_after_same_parent_reorg() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo {
            hash: B256::with_last_byte(1),
            number: 0,
            timestamp: 0,
            ..Default::default()
        };
        let next_a = BlockInfo {
            hash: B256::with_last_byte(2),
            number: 1,
            parent_hash: current.hash,
            timestamp: 12,
        };
        let next_b = BlockInfo { hash: B256::with_last_byte(3), ..next_a };
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.with_block(next_a);
        provider.set_chain_view(B256::with_last_byte(10));

        let mut selector = L1OriginSelector::new(Arc::clone(&cfg), provider);
        let mut unsafe_head = L2BlockInfo {
            block_info: BlockInfo::default(),
            l1_origin: NumHash { number: current.number, hash: current.hash },
            seq_num: 0,
        };

        let selected = selector.next_l1_origin(unsafe_head).await.unwrap();
        assert_eq!(selected, current);
        selector.await_inflight().await;
        assert_eq!(selector.next(), Some(next_a));

        selector.l1.replace_block(next_b);
        selector.l1.set_chain_view(B256::with_last_byte(11));
        unsafe_head.block_info.number = 5;
        unsafe_head.block_info.timestamp = 10;

        let selected = selector.next_l1_origin(unsafe_head).await.unwrap();
        assert_eq!(selected, current);
        selector.await_inflight().await;
        let selected = selector.next_l1_origin(unsafe_head).await.unwrap();
        assert_eq!(selected, next_b);
        assert_eq!(selector.next(), Some(next_b));
    }

    #[tokio::test]
    async fn test_next_l1_origin_waits_for_chain_view() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo {
            hash: B256::with_last_byte(1),
            number: 0,
            timestamp: 0,
            ..Default::default()
        };
        let next = BlockInfo {
            hash: B256::with_last_byte(2),
            number: 1,
            parent_hash: current.hash,
            timestamp: 2,
        };
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.with_block(next);
        provider.clear_chain_view();
        let mut selector = L1OriginSelector::new(Arc::clone(&cfg), provider);
        let unsafe_head = L2BlockInfo {
            block_info: BlockInfo::default(),
            l1_origin: NumHash { number: current.number, hash: current.hash },
            seq_num: 0,
        };

        let selected = selector.next_l1_origin(unsafe_head).await.unwrap();
        assert_eq!(selected, current);
        assert_eq!(selector.next(), None);

        selector.l1.set_chain_view(B256::with_last_byte(10));
        let selected = selector.next_l1_origin(unsafe_head).await.unwrap();
        assert_eq!(selected, current);
        selector.await_inflight().await;
        let selected = selector.next_l1_origin(unsafe_head).await.unwrap();
        assert_eq!(selected, next);
        assert_eq!(selector.next(), Some(next));
    }

    #[tokio::test]
    async fn test_next_l1_origin_discards_result_when_chain_view_changes_during_fetch() {
        #[cfg(feature = "metrics")]
        let recorder = DebuggingRecorder::new();
        #[cfg(feature = "metrics")]
        let snapshotter = recorder.snapshotter();
        #[cfg(feature = "metrics")]
        let _metrics_guard = metrics::set_default_local_recorder(&recorder);

        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo {
            hash: B256::with_last_byte(1),
            number: 0,
            timestamp: 0,
            ..Default::default()
        };
        let next_a = BlockInfo {
            hash: B256::with_last_byte(2),
            number: 1,
            parent_hash: current.hash,
            timestamp: 2,
        };
        let next_b = BlockInfo { hash: B256::with_last_byte(3), ..next_a };
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.with_block(next_a);
        provider.set_chain_view(B256::with_last_byte(10));
        provider.change_chain_view_after_number_fetch(Some(B256::with_last_byte(11)));
        let mut selector = L1OriginSelector::new(Arc::clone(&cfg), provider);
        let unsafe_head = L2BlockInfo {
            block_info: BlockInfo::default(),
            l1_origin: NumHash { number: current.number, hash: current.hash },
            seq_num: 0,
        };

        let selected = selector.next_l1_origin(unsafe_head).await.unwrap();
        assert_eq!(selected, current);
        selector.await_inflight().await;
        assert_eq!(selector.next(), None);

        selector.l1.replace_block(next_b);
        selector.l1.change_chain_view_after_number_fetch(None);
        let selected = selector.next_l1_origin(unsafe_head).await.unwrap();
        assert_eq!(selected, current);
        selector.await_inflight().await;
        let selected = selector.next_l1_origin(unsafe_head).await.unwrap();
        assert_eq!(selected, next_b);
        assert_eq!(selector.next(), Some(next_b));

        #[cfg(feature = "metrics")]
        assert!(snapshotter.snapshot().into_vec().iter().all(|(key, _, _, _)| {
            key.key().name() != "base_node.sequencer_l1_origin_orphans_total"
        }));
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
        let provider = MockOriginSelectorProvider::default();
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
        let _ = selector.next_l1_origin(unsafe_head).await;
        selector.await_inflight().await;
        let next = selector.next_l1_origin(unsafe_head).await.unwrap();

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

        let next = selector.next_l1_origin(unsafe_head).await.unwrap();

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

        let err = selector.next_l1_origin(unsafe_head).await.unwrap_err();

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
        let provider = MockOriginSelectorProvider::default();
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

        if next_available {
            let _ = selector.next_l1_origin(unsafe_head).await;
            selector.await_inflight().await;
            let next = selector.next_l1_origin(unsafe_head).await.unwrap();
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
            let next_err = selector.next_l1_origin(unsafe_head).await.unwrap_err();
            assert!(matches!(next_err, L1OriginSelectorError::NotEnoughData(_)));
        }
    }

    #[tokio::test]
    async fn test_next_origin_lookup_does_not_block_selection() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo {
            hash: B256::with_last_byte(1),
            number: 0,
            timestamp: 0,
            ..Default::default()
        };
        let next = BlockInfo {
            hash: B256::with_last_byte(2),
            number: 1,
            parent_hash: current.hash,
            timestamp: 12,
        };
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.with_block(next);
        provider.set_number_delay(Duration::from_millis(100));
        let mut selector = L1OriginSelector::new(cfg, provider);
        let unsafe_head = L2BlockInfo {
            l1_origin: NumHash { number: current.number, hash: current.hash },
            ..Default::default()
        };

        let selected = timeout(Duration::from_millis(20), selector.next_l1_origin(unsafe_head))
            .await
            .expect("background lookup must not block selection")
            .unwrap();
        assert_eq!(selected, current);

        selector.await_inflight().await;
        assert_eq!(selector.next(), Some(next));
    }

    #[tokio::test]
    async fn test_chain_view_change_replaces_inflight_fetch() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo {
            hash: B256::with_last_byte(1),
            number: 0,
            timestamp: 0,
            ..Default::default()
        };
        let next_a = BlockInfo {
            hash: B256::with_last_byte(2),
            number: 1,
            parent_hash: current.hash,
            timestamp: 12,
        };
        let next_b = BlockInfo { hash: B256::with_last_byte(3), ..next_a };
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.with_block(next_a);
        provider.set_chain_view(B256::with_last_byte(10));
        provider.set_number_delay(Duration::from_secs(1));
        let mut selector = L1OriginSelector::new(cfg, provider);
        let unsafe_head = L2BlockInfo {
            l1_origin: NumHash { number: current.number, hash: current.hash },
            ..Default::default()
        };

        let selected = selector.next_l1_origin(unsafe_head).await.unwrap();
        assert_eq!(selected, current);

        selector.l1.replace_block(next_b);
        selector.l1.set_chain_view(B256::with_last_byte(11));
        selector.l1.set_number_delay(Duration::ZERO);
        let selected = selector.next_l1_origin(unsafe_head).await.unwrap();
        assert_eq!(selected, current);
        selector.await_inflight().await;
        assert_eq!(selector.next(), Some(next_b));
    }

    #[tokio::test]
    async fn test_adopts_completed_fetch() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo {
            hash: B256::with_last_byte(1),
            number: 0,
            timestamp: 0,
            ..Default::default()
        };
        let next = BlockInfo {
            hash: B256::with_last_byte(2),
            number: 1,
            parent_hash: current.hash,
            timestamp: 2,
        };
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.with_block(next);
        let mut selector = L1OriginSelector::new(cfg, provider);
        let unsafe_head = L2BlockInfo {
            l1_origin: NumHash { number: current.number, hash: current.hash },
            ..Default::default()
        };

        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), current);
        selector.wait_for_inflight_completion().await;
        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), next);
        assert_eq!(selector.next(), Some(next));
    }

    #[tokio::test]
    async fn test_selected_origin_is_published() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo {
            hash: B256::with_last_byte(1),
            number: 0,
            timestamp: 0,
            ..Default::default()
        };
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        let mut selector = L1OriginSelector::new(cfg, provider);
        let selected_rx = selector.subscribe();
        let unsafe_head = L2BlockInfo {
            l1_origin: NumHash { number: current.number, hash: current.hash },
            ..Default::default()
        };

        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), current);
        let published = selected_rx.borrow().clone().expect("selected origin published");
        assert_eq!(published.block_info(), current);
        assert!(published.receipts.as_ref().is_some_and(|receipts| receipts.is_empty()));
    }

    #[tokio::test]
    async fn test_canonical_successor_with_wrong_parent_reports_orphan_and_clears_selection() {
        #[cfg(feature = "metrics")]
        let recorder = DebuggingRecorder::new();
        #[cfg(feature = "metrics")]
        let snapshotter = recorder.snapshotter();
        #[cfg(feature = "metrics")]
        let _metrics_guard = metrics::set_default_local_recorder(&recorder);

        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo { hash: B256::with_last_byte(1), number: 4, ..Default::default() };
        let orphaning_successor = BlockInfo {
            hash: B256::with_last_byte(2),
            parent_hash: B256::with_last_byte(99),
            number: 5,
            timestamp: 2,
        };
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.with_block(orphaning_successor);
        provider.set_chain_view(B256::with_last_byte(10));
        let mut selector = L1OriginSelector::new(cfg, provider);
        let selected_rx = selector.subscribe();
        let unsafe_head = L2BlockInfo {
            l1_origin: NumHash { number: current.number, hash: current.hash },
            ..Default::default()
        };

        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), current);
        selector.wait_for_inflight_completion().await;
        let error = selector.next_l1_origin(unsafe_head).await.unwrap_err();

        assert!(matches!(
            error,
            L1OriginSelectorError::NextL1OriginOrphaned { current: hash, next }
                if hash == current.hash && next == orphaning_successor.hash
        ));
        assert!(selected_rx.borrow().is_none());
        assert!(selector.next().is_none());
        #[cfg(feature = "metrics")]
        assert!(snapshotter.snapshot().into_vec().iter().any(|(key, _, _, value)| {
            key.key().name() == "base_node.sequencer_l1_origin_orphans_total"
                && value == &DebugValue::Counter(1)
        }));
    }

    #[tokio::test]
    async fn test_wrong_parent_without_receipts_still_reports_orphan() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo { hash: B256::with_last_byte(1), number: 4, ..Default::default() };
        let orphaning_successor = BlockInfo {
            hash: B256::with_last_byte(2),
            parent_hash: B256::with_last_byte(99),
            number: 5,
            timestamp: 2,
        };
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.with_block(orphaning_successor);
        provider.omit_receipts(orphaning_successor.number);
        provider.set_chain_view(B256::with_last_byte(10));
        let mut selector = L1OriginSelector::new(cfg, provider);
        let selected_rx = selector.subscribe();
        let unsafe_head = L2BlockInfo {
            l1_origin: NumHash { number: current.number, hash: current.hash },
            ..Default::default()
        };

        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), current);
        selector.wait_for_inflight_completion().await;
        let error = selector.next_l1_origin(unsafe_head).await.unwrap_err();

        assert!(matches!(
            error,
            L1OriginSelectorError::NextL1OriginOrphaned { current: hash, next }
                if hash == current.hash && next == orphaning_successor.hash
        ));
        assert!(selected_rx.borrow().is_none());
        assert!(selector.next().is_none());
    }

    #[tokio::test]
    async fn test_same_height_current_origin_reorg_is_detected_by_canonical_successor() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo { hash: B256::with_last_byte(1), number: 4, ..Default::default() };
        let replacement = BlockInfo {
            hash: B256::with_last_byte(2),
            parent_hash: current.parent_hash,
            ..current
        };
        let canonical_successor = BlockInfo {
            hash: B256::with_last_byte(3),
            parent_hash: replacement.hash,
            number: 5,
            timestamp: 2,
        };
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        let mut selector = L1OriginSelector::new(cfg, provider);
        let unsafe_head = L2BlockInfo {
            l1_origin: NumHash { number: current.number, hash: current.hash },
            ..Default::default()
        };

        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), current);
        selector.await_inflight().await;

        selector.l1.replace_block(replacement);
        selector.l1.set_chain_view(replacement.hash);
        assert_eq!(
            selector.next_l1_origin(unsafe_head).await.unwrap(),
            current,
            "without a canonical successor, sequencing may continue on the accepted origin"
        );

        selector.l1.with_block(canonical_successor);
        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), current);
        selector.wait_for_inflight_completion().await;
        let error = selector.next_l1_origin(unsafe_head).await.unwrap_err();

        assert!(matches!(
            error,
            L1OriginSelectorError::NextL1OriginOrphaned { current: hash, next }
                if hash == current.hash && next == canonical_successor.hash
        ));
    }

    #[tokio::test]
    async fn test_successor_without_receipts_is_not_adopted() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo { hash: B256::with_last_byte(1), number: 4, ..Default::default() };
        let successor = BlockInfo {
            hash: B256::with_last_byte(2),
            parent_hash: current.hash,
            number: 5,
            timestamp: 2,
        };
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.with_block(successor);
        provider.omit_receipts(successor.number);
        provider.set_chain_view(B256::with_last_byte(10));
        let mut selector = L1OriginSelector::new(cfg, provider);
        let unsafe_head = L2BlockInfo {
            l1_origin: NumHash { number: current.number, hash: current.hash },
            ..Default::default()
        };

        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), current);
        selector.wait_for_inflight_completion().await;
        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), current);
        assert!(selector.next().is_none());

        selector.l1.restore_receipts(successor.number);
        selector.wait_for_inflight_completion().await;
        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), successor);
        assert_eq!(selector.next(), Some(successor));
    }

    #[tokio::test]
    async fn test_reuses_prepared_current_when_provider_loses_current() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo {
            hash: B256::with_last_byte(1),
            number: 0,
            timestamp: 0,
            ..Default::default()
        };
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        let mut selector = L1OriginSelector::new(cfg, provider);
        let unsafe_head = L2BlockInfo {
            l1_origin: NumHash { number: current.number, hash: current.hash },
            ..Default::default()
        };

        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), current);
        selector.l1.remove_block(current.hash);

        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), current);
        assert_eq!(selector.current(), Some(current));
    }

    #[tokio::test]
    async fn test_accepted_head_promotes_ready_origin_after_chain_view_advances() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo {
            hash: B256::with_last_byte(1),
            number: 0,
            timestamp: 0,
            ..Default::default()
        };
        let next = BlockInfo {
            hash: B256::with_last_byte(2),
            number: 1,
            parent_hash: current.hash,
            timestamp: 2,
        };
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.with_block(next);
        let mut selector = L1OriginSelector::new(cfg, provider);
        let current_head = L2BlockInfo {
            l1_origin: NumHash { number: current.number, hash: current.hash },
            ..Default::default()
        };

        assert_eq!(selector.next_l1_origin(current_head).await.unwrap(), current);
        selector.await_inflight().await;
        assert_eq!(selector.next(), Some(next));

        // Simulate the next origin being selected and accepted, followed by an ordinary L1 head
        // advance and an L1 RPC outage before the child build starts.
        selector.l1.set_chain_view(B256::with_last_byte(3));
        selector.l1.remove_block(next.hash);
        let accepted_head = L2BlockInfo {
            block_info: BlockInfo { number: 1, timestamp: 2, ..Default::default() },
            l1_origin: NumHash { number: next.number, hash: next.hash },
            ..Default::default()
        };

        assert_eq!(selector.next_l1_origin(accepted_head).await.unwrap(), next);
        assert_eq!(selector.current(), Some(next));
    }

    #[tokio::test]
    async fn test_completed_fetch_retries_with_latest_chain_view() {
        let cfg = Arc::new(RollupConfig {
            block_time: 2,
            max_sequencer_drift: 600,
            ..Default::default()
        });
        let current = BlockInfo {
            hash: B256::with_last_byte(1),
            number: 0,
            timestamp: 0,
            ..Default::default()
        };
        let next = BlockInfo {
            hash: B256::with_last_byte(2),
            number: 1,
            parent_hash: current.hash,
            timestamp: 2,
        };
        let latest_view = B256::with_last_byte(11);
        let provider = MockOriginSelectorProvider::default();
        provider.with_block(current);
        provider.with_block(next);
        provider.set_chain_view(B256::with_last_byte(10));
        provider.change_chain_view_after_number_fetch(Some(latest_view));
        let mut selector = L1OriginSelector::new(cfg, provider);
        let unsafe_head = L2BlockInfo {
            l1_origin: NumHash { number: current.number, hash: current.hash },
            ..Default::default()
        };

        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), current);
        selector.wait_for_inflight_completion().await;
        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), current);
        assert!(matches!(
            &selector.next,
            NextSlot::InFlight { chain_view, .. } if *chain_view == latest_view
        ));

        selector.l1.change_chain_view_after_number_fetch(None);
        selector.wait_for_inflight_completion().await;
        assert_eq!(selector.next_l1_origin(unsafe_head).await.unwrap(), next);
    }
}
