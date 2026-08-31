//! Flashblocks state processor.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard},
    time::{Duration, Instant},
};

use alloy_consensus::{
    Block, BlockBody, BlockHeader, Header,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::{BlockNumberOrTag, Decodable2718};
use alloy_network::TransactionResponse;
use alloy_primitives::{Address, B256, BlockNumber};
use alloy_rpc_types_eth::state::StateOverride;
use arc_swap::ArcSwapOption;
use base_common_chains::Upgrades;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_common_flashblocks::Flashblock;
use base_execution_evm::{BaseEvmConfig, BaseNextBlockEnvAttributes};
use rayon::prelude::*;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::ConfigureEvm;
use reth_primitives_traits::{RecoveredBlock, SealedHeader};
use reth_provider::{BlockReaderIdExt, StateProviderBox, StateProviderFactory};
use reth_revm::{State, database::StateProviderDatabase};
use revm_database::states::bundle_state::BundleRetention;
use tokio::{
    sync::{Mutex, broadcast::Sender, mpsc::Receiver},
    time::{MissedTickBehavior, interval, sleep},
};

use crate::{
    AssembledBlock, BlockAssembler, ExecutionError, FlashblockCache, MAX_FLASHBLOCKS_PER_PAYLOAD,
    PendingBlocks, PendingBlocksBuilder, PendingStateBuilder, ProtocolError, ProviderError, Result,
    StateProcessorError,
    metrics::Metrics,
    validation::{
        CanonicalBlockReconciler, FlashblockSequenceValidator, ReconciliationStrategy,
        ReorgDetector, SequenceValidationResult,
    },
};

type PendingExecutionDb = State<StateProviderDatabase<StateProviderBox>>;

#[derive(Debug)]
struct LivePendingState {
    db: PendingExecutionDb,
    state_overrides: StateOverride,
}

/// Messages consumed by the state processor.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum StateUpdate {
    /// New canonical block to reconcile against pending state.
    Canonical(RecoveredBlock<BaseBlock>),
    /// Incoming flashblock payload to extend pending state.
    Flashblock(Flashblock),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdatePreflight {
    Process,
    ResumeRecovery,
    Skip,
    EnterRecovery,
}

impl StateUpdate {
    /// Returns the canonical block number on which this update is anchored.
    pub fn pending_anchor_number(&self) -> u64 {
        match self {
            Self::Canonical(block) => block.number,
            Self::Flashblock(flashblock) => flashblock.metadata.block_number.saturating_sub(1),
        }
    }

    /// Returns `true` for an index-zero flashblock built on the exact current canonical tip.
    pub fn is_recovery_resume(&self, best_number: u64, best_hash: B256) -> bool {
        match self {
            Self::Flashblock(flashblock) => {
                flashblock.index == 0
                    && flashblock.metadata.block_number == best_number.saturating_add(1)
                    && flashblock.base.as_ref().is_some_and(|base| base.parent_hash == best_hash)
            }
            Self::Canonical(_) => false,
        }
    }

    fn flashblock_has_wrong_parent_at_tip(
        flashblock: &Flashblock,
        best_number: u64,
        best_hash: B256,
    ) -> bool {
        flashblock.metadata.block_number.saturating_sub(1) == best_number
            && flashblock.index == 0
            && flashblock.base.as_ref().is_none_or(|base| base.parent_hash != best_hash)
    }

    fn is_stale_against(&self, best_number: u64, best_hash: B256) -> bool {
        if self.pending_anchor_number() < best_number {
            return true;
        }

        match self {
            Self::Canonical(block) => block.number == best_number && block.hash() != best_hash,
            Self::Flashblock(flashblock) => {
                Self::flashblock_has_wrong_parent_at_tip(flashblock, best_number, best_hash)
            }
        }
    }

    fn preflight(
        &self,
        best: (u64, B256),
        has_pending: bool,
        pending_is_based_on_best: bool,
        recovering: bool,
    ) -> UpdatePreflight {
        let (best_number, best_hash) = best;

        if recovering {
            if self.is_recovery_resume(best_number, best_hash) {
                return UpdatePreflight::ResumeRecovery;
            }

            return match self {
                Self::Canonical(block)
                    if block.number == best_number && block.hash() == best_hash =>
                {
                    UpdatePreflight::Process
                }
                Self::Flashblock(_) if self.pending_anchor_number() > best_number => {
                    UpdatePreflight::Process
                }
                _ => UpdatePreflight::Skip,
            };
        }

        // A canonical notification can race ahead of provider visibility. The existing snapshot
        // is already stale once the notification arrives, so clear it and wait for a tip child.
        if matches!(self, Self::Canonical(block) if block.number > best_number) {
            return UpdatePreflight::EnterRecovery;
        }

        // A canonical tip update can rebase pending. A flashblock cannot safely extend a snapshot
        // that is anchored behind the provider.
        if matches!(self, Self::Flashblock(_)) && has_pending && !pending_is_based_on_best {
            return if self.is_recovery_resume(best_number, best_hash) {
                UpdatePreflight::ResumeRecovery
            } else {
                UpdatePreflight::EnterRecovery
            };
        }

        if self.is_stale_against(best_number, best_hash) {
            return if pending_is_based_on_best {
                UpdatePreflight::Skip
            } else {
                UpdatePreflight::EnterRecovery
            };
        }

        UpdatePreflight::Process
    }
}

/// Processes flashblocks and canonical blocks to keep pending state updated.
#[derive(Debug)]
pub struct StateProcessor<Client> {
    rx: Arc<Mutex<Receiver<(StateUpdate, u64)>>>,
    pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
    client: Client,
    sender: Sender<Arc<PendingBlocks>>,
    cache: StdMutex<FlashblockCache>,
    live_state: StdMutex<Option<LivePendingState>>,
    max_pending_blocks_depth: u64,
    recovery_epoch: Arc<StdMutex<u64>>,
}

impl<Client> StateProcessor<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + Upgrades>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + 'static,
{
    fn lock_live_state(&self) -> StdMutexGuard<'_, Option<LivePendingState>> {
        self.live_state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_cache(&self) -> StdMutexGuard<'_, FlashblockCache> {
        self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn clear_live_state(&self) {
        *self.lock_live_state() = None;
    }

    fn set_live_state(&self, db: PendingExecutionDb, state_overrides: StateOverride) {
        *self.lock_live_state() = Some(LivePendingState { db, state_overrides });
    }

    fn publish_pending_blocks(
        &self,
        mut pending_blocks_builder: PendingBlocksBuilder,
        mut db: PendingExecutionDb,
        state_overrides: StateOverride,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        db.merge_transitions(BundleRetention::Reverts);
        pending_blocks_builder.with_bundle_state(db.bundle_state.clone());
        pending_blocks_builder.with_state_overrides(state_overrides.clone());

        let pending_blocks = Arc::new(pending_blocks_builder.build()?);
        self.set_live_state(db, state_overrides);

        Ok(Some(pending_blocks))
    }

    /// Creates a new state processor wired to the provided channels and state.
    pub fn new(
        client: Client,
        pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
        max_pending_blocks_depth: u64,
        recovery_epoch: Arc<StdMutex<u64>>,
        rx: Arc<Mutex<Receiver<(StateUpdate, u64)>>>,
        sender: Sender<Arc<PendingBlocks>>,
    ) -> Self {
        let cache = client
            .best_block_number()
            .map_or_else(|_| FlashblockCache::new(0), FlashblockCache::new);

        Self {
            pending_blocks,
            client,
            rx,
            sender,
            cache: StdMutex::new(cache),
            live_state: StdMutex::new(None),
            max_pending_blocks_depth,
            recovery_epoch,
        }
    }

    fn best_canonical_header(&self) -> Option<SealedHeader> {
        self.client.sealed_header_by_number_or_tag(BlockNumberOrTag::Latest).ok().flatten()
    }

    fn pending_tracks_canonical_tip(&self, pending: &PendingBlocks, best: &SealedHeader) -> bool {
        pending.is_based_on_canonical(best.number(), best.hash())
            && pending.latest_block_number().saturating_sub(best.number())
                <= self.max_pending_blocks_depth
    }

    fn invalidate_pending_for_recovery(&self, best_number: u64) {
        warn!(best = best_number, "flashblock processor entering recovery");
        Metrics::pending_recovery_transitions().increment(1);
        self.pending_blocks.swap(None);
        self.clear_live_state();
    }

    fn enter_recovery(&self, best_number: u64) {
        self.invalidate_pending_for_recovery(best_number);
        *self.lock_cache() = FlashblockCache::new(best_number);
    }

    fn enter_recovery_preserving_cache(&self, best_number: u64) {
        self.invalidate_pending_for_recovery(best_number);
        self.lock_cache().update_canonical(best_number);
    }

    async fn preflight_update(
        &self,
        update: &StateUpdate,
        enqueued_generation: u64,
        observed_generation: &mut u64,
        deferred_canonical: &mut Option<(RecoveredBlock<BaseBlock>, u64)>,
        recovering: &mut bool,
    ) -> Option<(SealedHeader, bool, u64)> {
        let mut provider_retries = 0;
        let best = loop {
            if let Some(best) = self.best_canonical_header() {
                break best;
            }
            if provider_retries >= 20 {
                let last_canonical = self.lock_cache().latest_canonical_number();
                self.enter_recovery(last_canonical);
                *recovering = true;
                Metrics::pending_stale_events_skipped().increment(1);
                return None;
            }
            provider_retries += 1;
            sleep(Duration::from_millis(25)).await;
        };
        let generation =
            *self.recovery_epoch.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *observed_generation != generation {
            *observed_generation = generation;
            self.enter_recovery(best.number());
            *recovering = true;
        }
        if enqueued_generation != generation {
            Metrics::pending_stale_events_skipped().increment(1);
            return None;
        }
        if let StateUpdate::Flashblock(flashblock) = update
            && let Some((block, _)) = deferred_canonical.as_ref()
            && update.pending_anchor_number() <= block.number
        {
            if update.pending_anchor_number() == block.number {
                self.lock_cache().insert(flashblock.clone());
            }
            Metrics::pending_stale_events_skipped().increment(1);
            return None;
        }

        let pending_blocks = self.pending_blocks.load_full();
        let pending_is_based_on_best = pending_blocks
            .as_ref()
            .is_some_and(|pending| self.pending_tracks_canonical_tip(pending, &best));
        if let StateUpdate::Flashblock(flashblock) = update
            && (flashblock.metadata.block_number == 0
                || flashblock.base.as_ref().is_some_and(|base| base.block_number == 0))
        {
            Metrics::pending_stale_events_skipped().increment(1);
            return None;
        }
        if let StateUpdate::Flashblock(flashblock) = update
            && flashblock.index >= MAX_FLASHBLOCKS_PER_PAYLOAD
        {
            let extends_accepted_payload = pending_blocks.as_ref().is_some_and(|pending| {
                flashblock.metadata.block_number == pending.latest_block_number()
                    && flashblock.payload_id == pending.latest_payload_id()
            });
            if extends_accepted_payload {
                self.enter_recovery(best.number());
                *recovering = true;
            }
            Metrics::pending_stale_events_skipped().increment(1);
            return None;
        }
        if let StateUpdate::Flashblock(flashblock) = update
            && flashblock.index == 0
            && flashblock
                .base
                .as_ref()
                .is_none_or(|base| base.block_number != flashblock.metadata.block_number)
        {
            Metrics::pending_stale_events_skipped().increment(1);
            return None;
        }
        if let (StateUpdate::Flashblock(flashblock), Some(pending)) =
            (update, pending_blocks.as_ref())
        {
            if pending_is_based_on_best
                && flashblock.index > 0
                && pending.payload_id_for_block(flashblock.metadata.block_number).is_none()
            {
                Metrics::pending_stale_events_skipped().increment(1);
                return None;
            }

            if pending_is_based_on_best
                && let Some(payload_id) =
                    pending.payload_id_for_block(flashblock.metadata.block_number)
            {
                if flashblock.payload_id == payload_id
                    && let Some(existing) = pending.flashblock_for_identity(
                        flashblock.metadata.block_number,
                        flashblock.payload_id,
                        flashblock.index,
                    )
                {
                    if existing != flashblock {
                        self.enter_recovery(best.number());
                        *recovering = true;
                    }
                    Metrics::pending_stale_events_skipped().increment(1);
                    return None;
                }
                if flashblock.index == 0 {
                    if flashblock.base.as_ref().is_none_or(|base| {
                        pending.expected_parent_hash(flashblock.metadata.block_number)
                            != Some(base.parent_hash)
                    }) {
                        Metrics::pending_stale_events_skipped().increment(1);
                        return None;
                    }
                    if flashblock.payload_id != payload_id {
                        return Some((best, false, generation));
                    }
                }
                if flashblock.payload_id != payload_id || flashblock.index == 0 {
                    Metrics::pending_stale_events_skipped().increment(1);
                    return None;
                }
            }

            if pending_is_based_on_best
                && flashblock.index == 0
                && flashblock.metadata.block_number
                    == pending.latest_block_number().saturating_add(1)
                && flashblock
                    .base
                    .as_ref()
                    .is_none_or(|base| base.parent_hash != pending.latest_block_hash())
            {
                Metrics::pending_stale_events_skipped().increment(1);
                return None;
            }
        }

        let preflight = update.preflight(
            (best.number(), best.hash()),
            pending_blocks.is_some(),
            pending_is_based_on_best,
            *recovering,
        );

        match preflight {
            UpdatePreflight::Process => Some((best, false, generation)),
            UpdatePreflight::ResumeRecovery => {
                if pending_blocks.is_some() && !pending_is_based_on_best {
                    self.enter_recovery(best.number());
                    *recovering = true;
                }
                Some((best, true, generation))
            }
            UpdatePreflight::Skip => {
                Metrics::pending_stale_events_skipped().increment(1);
                None
            }
            UpdatePreflight::EnterRecovery => {
                let canonical_ahead = matches!(
                    update,
                    StateUpdate::Canonical(block) if block.number > best.number()
                );
                if let StateUpdate::Canonical(block) = update
                    && canonical_ahead
                {
                    *deferred_canonical = Some((block.clone(), enqueued_generation));
                }
                if canonical_ahead {
                    self.enter_recovery_preserving_cache(best.number());
                } else {
                    self.enter_recovery(best.number());
                }
                *recovering = true;
                Metrics::pending_stale_events_skipped().increment(1);
                None
            }
        }
    }

    /// Processes updates from the queue until the channel closes.
    pub async fn start(&self) {
        let mut recovering = false;
        let mut observed_generation =
            *self.recovery_epoch.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut deferred_canonical: Option<(RecoveredBlock<BaseBlock>, u64)> = None;
        let mut replay_updates = VecDeque::new();
        let mut deferred_interval = interval(Duration::from_millis(100));
        deferred_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            let (update, enqueued_generation) = if let Some(replay) = replay_updates.pop_front() {
                replay
            } else {
                tokio::select! {
                    update = async { self.rx.lock().await.recv().await } => {
                        let Some(update) = update else {
                            break;
                        };
                        update
                    }
                    _ = deferred_interval.tick(), if deferred_canonical.is_some() => {
                        let Some((deferred, _)) = deferred_canonical.as_ref() else {
                            continue;
                        };
                        if self
                            .best_canonical_header()
                            .is_none_or(|best| best.number() < deferred.number)
                        {
                            continue;
                        }
                        let Some(update) = deferred_canonical
                            .take()
                            .map(|(block, generation)| (StateUpdate::Canonical(block), generation))
                        else {
                            continue;
                        };
                        update
                    }
                }
            };
            let Some((best, resuming_recovery, generation)) = self
                .preflight_update(
                    &update,
                    enqueued_generation,
                    &mut observed_generation,
                    &mut deferred_canonical,
                    &mut recovering,
                )
                .await
            else {
                continue;
            };

            let prev_pending_blocks = self.pending_blocks.load_full();
            match update {
                StateUpdate::Canonical(block) => {
                    debug!(message = "processing canonical block", block_number = block.number);
                    match self.process_canonical_block(prev_pending_blocks, &block) {
                        Ok((new_pending_blocks, requires_recovery)) => {
                            if requires_recovery {
                                self.enter_recovery(best.number());
                                recovering = true;
                                continue;
                            }

                            if let Some(pending) = &new_pending_blocks {
                                let Some(current_best) = self.best_canonical_header() else {
                                    self.enter_recovery(best.number());
                                    recovering = true;
                                    continue;
                                };
                                if !self.pending_tracks_canonical_tip(pending, &current_best) {
                                    self.enter_recovery(current_best.number());
                                    recovering = true;
                                    continue;
                                }
                            }

                            let stale_generation = {
                                let epoch = self
                                    .recovery_epoch
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                if *epoch != generation {
                                    true
                                } else {
                                    self.pending_blocks.swap(new_pending_blocks);
                                    false
                                }
                            };
                            if stale_generation {
                                self.enter_recovery(best.number());
                                recovering = true;
                                continue;
                            }

                            let cached = {
                                let mut cache = self.lock_cache();
                                let cache_rolled_back = cache.latest_canonical_number()
                                    > block.number
                                    && block.number == best.number()
                                    && block.hash() == best.hash();
                                if cache_rolled_back {
                                    *cache = FlashblockCache::new(block.number);
                                }
                                let should_advance =
                                    block.number >= cache.latest_canonical_number();
                                should_advance.then(|| {
                                    cache.update_canonical(block.number);
                                    let cached_block_number = block.number.saturating_add(1);
                                    (
                                        cached_block_number,
                                        cache.drain(cached_block_number, block.hash()),
                                    )
                                })
                            };
                            if let Some((cached_block_number, mut cached)) = cached {
                                if self.pending_blocks.load_full().is_some_and(|pending| {
                                    pending.latest_block_number() >= cached_block_number
                                }) {
                                    cached.clear();
                                }

                                if !cached.is_empty() {
                                    debug!(
                                        message =
                                            "replaying cached flashblocks after canonical block",
                                        canonical_block = block.number,
                                        cached_count = cached.len(),
                                    );
                                    let replay_generation = *self
                                        .recovery_epoch
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                                    replay_updates.extend(cached.into_iter().map(|flashblock| {
                                        (StateUpdate::Flashblock(flashblock), replay_generation)
                                    }));
                                }
                            }
                        }
                        Err(e) => {
                            error!(message = "could not process canonical block", error = %e);
                            self.enter_recovery(best.number());
                            recovering = true;
                        }
                    }
                }
                StateUpdate::Flashblock(flashblock) => {
                    debug!(
                        message = "processing flashblock",
                        block_number = flashblock.metadata.block_number,
                        flashblock_index = flashblock.index
                    );
                    let (applied, requires_recovery) = self
                        .apply_flashblock(
                            prev_pending_blocks,
                            flashblock,
                            !resuming_recovery,
                            &best,
                            generation,
                        )
                        .await;
                    if requires_recovery {
                        recovering = true;
                    } else if applied {
                        recovering = false;
                    }
                }
            }
        }
    }

    async fn apply_flashblock(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblock: Flashblock,
        cache_on_missing_canonical_header: bool,
        expected_best: &SealedHeader,
        expected_generation: u64,
    ) -> (bool, bool) {
        let start_time = Instant::now();
        let mut provider_retries = 0;
        let result = loop {
            let result = self.process_flashblock(prev_pending_blocks.clone(), &flashblock);
            if matches!(
                result,
                Err(StateProcessorError::Provider(ProviderError::MissingCanonicalHeader { .. }))
            ) && provider_retries < 20
            {
                provider_retries += 1;
                sleep(Duration::from_millis(25)).await;
                continue;
            }
            break result;
        };

        match result {
            Ok(new_pending_blocks) => {
                let applied = new_pending_blocks.is_some();
                if let Some(ref pb) = new_pending_blocks {
                    let Some(current_best) = self.best_canonical_header() else {
                        self.enter_recovery(expected_best.number());
                        return (false, true);
                    };
                    if !self.pending_tracks_canonical_tip(pb, &current_best) {
                        self.enter_recovery(current_best.number());
                        return (false, true);
                    }
                }
                let stale_generation = {
                    let epoch =
                        self.recovery_epoch.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    if *epoch != expected_generation {
                        true
                    } else {
                        if let Some(ref pb) = new_pending_blocks {
                            _ = self.sender.send(Arc::clone(pb));
                        }
                        self.pending_blocks.swap(new_pending_blocks);
                        false
                    }
                };
                if stale_generation {
                    self.enter_recovery(expected_best.number());
                    return (false, true);
                }
                Metrics::block_processing_duration().record(start_time.elapsed());
                (applied, false)
            }
            Err(e) => {
                match e {
                    StateProcessorError::Provider(ProviderError::MissingCanonicalHeader {
                        ..
                    }) if cache_on_missing_canonical_header => {
                        let inserted = self.lock_cache().insert(flashblock);
                        if inserted {
                            debug!(message = "cached flashblock pending canonical block", error = %e);
                        }
                        return (false, false);
                    }
                    StateProcessorError::MissingFirstFlashblock => {
                        let mut cache = self.lock_cache();
                        // this error should only occur for non-zero index flashblocks, but check here for index safety
                        if flashblock.index > 0
                            && cache.has_flashblock(
                                flashblock.metadata.block_number,
                                flashblock.payload_id,
                                flashblock.index - 1,
                            )
                            && cache.insert(flashblock)
                        {
                            return (false, false);
                        }
                        // we should ignore this error since it doesn't necessarily indicate a problem
                        return (false, false);
                    }
                    StateProcessorError::Provider(_) => {
                        error!(message = "provider remained unavailable while processing flashblock", error = %e);
                        self.enter_recovery(expected_best.number());
                        return (false, true);
                    }
                    _ => {}
                }

                // skip logging expected caching case
                if !matches!(
                    e,
                    StateProcessorError::Provider(ProviderError::MissingCanonicalHeader { .. })
                ) {
                    error!(message = "could not process Flashblock", error = %e);
                    Metrics::block_processing_error().increment(1);
                }
                (false, false)
            }
        }
    }

    #[instrument(level = "debug", skip_all, fields(block_number = block.number))]
    fn process_canonical_block(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        block: &RecoveredBlock<BaseBlock>,
    ) -> Result<(Option<Arc<PendingBlocks>>, bool)> {
        let pending_blocks = match &prev_pending_blocks {
            Some(pb) => pb,
            None => {
                debug!(message = "no pending state to update with canonical block, skipping");
                self.clear_live_state();
                return Ok((None, false));
            }
        };

        let mut flashblocks = pending_blocks.get_flashblocks();
        let num_flashblocks_for_canon =
            flashblocks.iter().filter(|fb| fb.metadata.block_number == block.number).count();
        Metrics::flashblocks_in_block().record(num_flashblocks_for_canon as f64);
        Metrics::pending_snapshot_height().set(pending_blocks.latest_block_number() as f64);

        let earliest = pending_blocks.earliest_block_number();
        let latest = pending_blocks.latest_block_number();
        let anchor = earliest.saturating_sub(1);
        let in_pending_interval = block.number >= earliest && block.number <= latest;
        let is_pending_anchor = block.number == anchor;

        // Only compare transaction sets for blocks represented in pending. An older queued
        // canonical has no tracked transactions and must not look like a reorg.
        let reorg_detected = if in_pending_interval {
            let tracked_txn_hashes: Vec<_> = pending_blocks
                .get_transactions_for_block(block.number)
                .map(|tx| tx.tx_hash())
                .collect();
            let block_txn_hashes: Vec<_> =
                block.body().transactions().map(|tx| tx.tx_hash()).collect();
            let reorg_result = ReorgDetector::detect(&tracked_txn_hashes, &block_txn_hashes);
            if reorg_result.is_reorg() {
                warn!(
                    tracked_txn_hashes = ?tracked_txn_hashes,
                    block_txn_hashes = ?block_txn_hashes,
                    "reorg detected, clearing pending flashblocks"
                );
            }
            reorg_result.is_reorg()
        } else if is_pending_anchor {
            let pending_parent = pending_blocks.parent_hash();
            let canonical_hash = block.hash();
            if pending_parent != canonical_hash {
                warn!(
                    pending_parent_hash = %pending_parent,
                    canonical_hash = %canonical_hash,
                    canonical_block = block.number,
                    "pending anchor hash mismatch, clearing pending flashblocks"
                );
                true
            } else {
                false
            }
        } else {
            false
        };

        let strategy =
            CanonicalBlockReconciler::reconcile(earliest, latest, block.number, reorg_detected);

        let requires_recovery = matches!(strategy, ReconciliationStrategy::HandleReorg);
        let pending_blocks = match strategy {
            ReconciliationStrategy::CatchUp => {
                debug!(
                    message = "pending snapshot cleared because canonical caught up",
                    latest_pending_block = latest,
                    canonical_block = block.number,
                );
                Metrics::pending_clear_catchup().increment(1);
                Metrics::pending_snapshot_fb_index()
                    .set(pending_blocks.latest_flashblock_index() as f64);
                self.clear_live_state();
                Ok(None)
            }
            ReconciliationStrategy::HandleReorg => {
                Metrics::pending_clear_reorg().increment(1);
                self.clear_live_state();
                Ok(None)
            }
            ReconciliationStrategy::Rebase => {
                flashblocks.retain(|flashblock| flashblock.metadata.block_number > block.number);
                if flashblocks.is_empty() {
                    Metrics::pending_clear_catchup().increment(1);
                    self.clear_live_state();
                    Ok(None)
                } else if flashblocks.first().is_some_and(|flashblock| {
                    flashblock.index != 0
                        || flashblock.metadata.block_number != block.number.saturating_add(1)
                        || flashblock
                            .base
                            .as_ref()
                            .is_none_or(|base| base.parent_hash != block.hash())
                }) {
                    warn!(
                        canonical_block = block.number,
                        "pending suffix does not start from canonical tip"
                    );
                    self.clear_live_state();
                    return Ok((None, true));
                } else {
                    debug!(
                        message = "rebasing pending flashblocks onto canonical block",
                        canonical_block = block.number,
                        remaining_flashblocks = flashblocks.len(),
                    );
                    self.clear_live_state();
                    self.build_pending_state(None, &flashblocks)
                }
            }
            ReconciliationStrategy::Keep => {
                debug!(
                    message = "canonical block does not require pending rebuild",
                    latest_pending_block = latest,
                    earliest_pending_block = earliest,
                    canonical_block = block.number,
                    strategy = ?strategy,
                );
                Ok(prev_pending_blocks)
            }
        }?;

        Ok((pending_blocks, requires_recovery))
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(
            block_number = flashblock.metadata.block_number,
            flashblock_index = flashblock.index
        )
    )]
    fn process_flashblock(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblock: &Flashblock,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        let pending_blocks = match &prev_pending_blocks {
            Some(pb) => pb,
            None => {
                if flashblock.index == 0 {
                    return self.build_pending_state(None, std::slice::from_ref(flashblock));
                }

                return Err(StateProcessorError::MissingFirstFlashblock);
            }
        };

        if flashblock.index == 0
            && let Some(payload_id) =
                pending_blocks.payload_id_for_block(flashblock.metadata.block_number)
        {
            if flashblock.payload_id == payload_id {
                return Ok(prev_pending_blocks);
            }
            let mut flashblocks = pending_blocks.get_flashblocks();
            flashblocks.retain(|existing| {
                existing.metadata.block_number < flashblock.metadata.block_number
            });
            flashblocks.push(flashblock.clone());
            self.clear_live_state();
            return self.build_pending_state(None, &flashblocks);
        }

        let validation_result = FlashblockSequenceValidator::validate(
            pending_blocks.latest_block_number(),
            pending_blocks.latest_flashblock_index(),
            flashblock.metadata.block_number,
            flashblock.index,
            flashblock.metadata.prev_flashblock_id,
        );

        match validation_result {
            SequenceValidationResult::NextInSequence => {
                self.build_pending_state_for_same_block(pending_blocks, flashblock)
            }
            SequenceValidationResult::FirstOfNextBlock => {
                self.build_pending_state_for_next_block(pending_blocks, flashblock)
            }
            SequenceValidationResult::Duplicate => {
                // We have received a duplicate flashblock for the current block
                Metrics::unexpected_block_order().increment(1);
                warn!(
                    message = "Received duplicate Flashblock for current block, ignoring",
                    curr_block = %pending_blocks.latest_block_number(),
                    flashblock_index = %flashblock.index,
                );
                Ok(prev_pending_blocks)
            }
            SequenceValidationResult::InvalidNewBlockIndex { block_number, index: _ } => {
                // We have received a non-zero flashblock for a new block
                Metrics::unexpected_block_order().increment(1);
                error!(
                    message = "Received non-zero index Flashblock for new block, zeroing Flashblocks until we receive a base Flashblock",
                    curr_block = %pending_blocks.latest_block_number(),
                    new_block = %block_number,
                );
                self.clear_live_state();
                Ok(None)
            }
            SequenceValidationResult::NonSequentialGap { expected, actual } => {
                Metrics::unexpected_block_order().increment(1);
                error!(
                    curr_block = %pending_blocks.latest_block_number(),
                    expected_flashblock_index = %expected,
                    actual_flashblock_index = %actual,
                    "received non-sequential flashblock index for current block"
                );
                self.clear_live_state();
                Ok(None)
            }
            SequenceValidationResult::NonSequentialPredecessor { expected, actual } => {
                Metrics::unexpected_block_order().increment(1);
                error!(
                    curr_block = %pending_blocks.latest_block_number(),
                    curr_flashblock_index = %pending_blocks.latest_flashblock_index(),
                    new_block = %flashblock.metadata.block_number,
                    new_flashblock_index = %flashblock.index,
                    expected_prev_block = %expected.block_number,
                    expected_prev_index = %expected.index,
                    actual_prev_block = %actual.block_number,
                    actual_prev_index = %actual.index,
                    "received flashblock with non-sequential predecessor link"
                );
                self.clear_live_state();
                Ok(None)
            }
        }
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(
            block_number = flashblock.metadata.block_number,
            flashblock_index = flashblock.index
        )
    )]
    fn build_pending_state_for_same_block(
        &self,
        prev_pending_blocks: &Arc<PendingBlocks>,
        flashblock: &Flashblock,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        let latest_block_base = prev_pending_blocks.latest_block_base().clone();
        let latest_block_l1_block_info = prev_pending_blocks.latest_block_l1_block_info().clone();
        let latest_flashblock_tx_start = prev_pending_blocks.pending_transaction_count();

        let mut live_state = self.lock_live_state();
        let Some(LivePendingState { mut db, state_overrides }) = live_state.take() else {
            drop(live_state);
            warn!(
                message = "live pending state unavailable, falling back to full rebuild",
                block_number = flashblock.metadata.block_number,
                flashblock_index = flashblock.index,
                path = "same_block"
            );
            let mut flashblocks = prev_pending_blocks.get_flashblocks();
            flashblocks.push(flashblock.clone());
            return self.build_pending_state(Some(Arc::clone(prev_pending_blocks)), &flashblocks);
        };
        drop(live_state);

        let latest_header = prev_pending_blocks.latest_header();
        let mut latest_block_flashblocks = prev_pending_blocks.latest_block_flashblocks();
        latest_block_flashblocks.push(flashblock.clone());
        let latest_block_header =
            BlockAssembler::refresh_same_block_header(&latest_header, &latest_block_flashblocks)?;

        db.block_hashes.insert(latest_block_base.block_number - 1, latest_block_base.parent_hash);

        let evm_config = BaseEvmConfig::base(self.client.chain_spec());
        let evm_env = evm_config
            .evm_env(&latest_header)
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
        let evm = evm_config.evm_with_env(db, evm_env);

        let previous_block_transaction_count = prev_pending_blocks.latest_block_transaction_count();
        let pending_block = Block {
            header: Header {
                parent_hash: latest_block_base.parent_hash,
                number: latest_block_base.block_number,
                timestamp: latest_block_base.timestamp,
                gas_limit: latest_block_base.gas_limit,
                base_fee_per_gas: Some(latest_block_base.base_fee_per_gas.saturating_to()),
                ..Default::default()
            },
            body: BlockBody {
                transactions: flashblock
                    .diff
                    .transactions
                    .iter()
                    .map(|tx| BaseTxEnvelope::decode_2718_exact(tx.as_ref()))
                    .collect::<std::result::Result<_, _>>()
                    .map_err(|e| ExecutionError::BlockConversion(e.to_string()))?,
                ..Default::default()
            },
        };
        let latest_block_transaction_count = prev_pending_blocks.latest_block_transaction_count()
            + pending_block.body.transactions.len();
        let recovery_start = Instant::now();
        let txs_with_senders: Vec<(BaseTxEnvelope, Address)> = pending_block
            .body
            .transactions
            .par_iter()
            .cloned()
            .map(|tx| -> Result<(BaseTxEnvelope, Address)> {
                let sender = tx.recover_signer()?;
                Ok((tx, sender))
            })
            .collect::<Result<_>>()?;
        let sender_recovery_elapsed = recovery_start.elapsed();
        Metrics::sender_recovery_duration().record(sender_recovery_elapsed);

        let mut pending_blocks_builder = PendingBlocksBuilder::from_previous(prev_pending_blocks);
        pending_blocks_builder.with_flashblocks([flashblock.clone()]);
        pending_blocks_builder.replace_latest_header(latest_block_header);

        let mut pending_state_builder = PendingStateBuilder::new(
            self.client.chain_spec(),
            evm,
            pending_block,
            None,
            latest_block_l1_block_info.clone(),
            state_overrides,
        );
        pending_state_builder.set_execution_offsets(
            prev_pending_blocks.latest_block_cumulative_gas_used(),
            prev_pending_blocks.latest_block_next_log_index(),
        );

        for (offset, (transaction, sender)) in txs_with_senders.into_iter().enumerate() {
            let tx_hash = transaction.tx_hash();
            let idx = previous_block_transaction_count + offset;

            pending_blocks_builder.with_transaction_sender(tx_hash, sender);
            pending_blocks_builder.increment_nonce(sender);

            let recovered_transaction = Recovered::new_unchecked(transaction, sender);
            let executed_transaction =
                pending_state_builder.execute_transaction(idx, recovered_transaction)?;

            if let Some(time_us) = executed_transaction.execution_time_us {
                pending_blocks_builder.with_execution_time(tx_hash, time_us);
            }

            for (address, account) in &executed_transaction.state {
                if account.is_touched() {
                    pending_blocks_builder.with_account_balance(*address, account.info.balance);
                }
            }

            pending_blocks_builder.with_transaction(executed_transaction.rpc_transaction);
            pending_blocks_builder.with_receipt(tx_hash, executed_transaction.receipt);
            pending_blocks_builder.with_transaction_state(tx_hash, executed_transaction.state);
            pending_blocks_builder.with_transaction_result(tx_hash, executed_transaction.result);
        }

        let latest_block_cumulative_gas_used = pending_state_builder.cumulative_gas_used();
        let latest_block_next_log_index = pending_state_builder.next_log_index();
        let (db, state_overrides) = pending_state_builder.into_db_and_state_overrides();
        pending_blocks_builder.with_latest_block_context(
            latest_flashblock_tx_start,
            latest_block_base,
            latest_block_l1_block_info,
            latest_block_transaction_count,
            latest_block_cumulative_gas_used,
            latest_block_next_log_index,
        );
        self.publish_pending_blocks(pending_blocks_builder, db, state_overrides)
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(
            block_number = flashblock.metadata.block_number,
            flashblock_index = flashblock.index
        )
    )]
    fn build_pending_state_for_next_block(
        &self,
        prev_pending_blocks: &Arc<PendingBlocks>,
        flashblock: &Flashblock,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        let Some(base) = flashblock.base.clone() else {
            return Err(StateProcessorError::MissingFirstFlashblock);
        };
        let expected_parent = prev_pending_blocks.latest_block_hash();
        if base.parent_hash != expected_parent {
            return Err(ProtocolError::ParentHashMismatch {
                expected: expected_parent,
                actual: base.parent_hash,
            }
            .into());
        }

        let mut live_state = self.lock_live_state();
        let Some(LivePendingState { mut db, state_overrides }) = live_state.take() else {
            drop(live_state);
            warn!(
                message = "live pending state unavailable, falling back to full rebuild",
                block_number = flashblock.metadata.block_number,
                flashblock_index = flashblock.index,
                path = "next_block"
            );
            let mut flashblocks = prev_pending_blocks.get_flashblocks();
            flashblocks.push(flashblock.clone());
            return self.build_pending_state(Some(Arc::clone(prev_pending_blocks)), &flashblocks);
        };
        drop(live_state);

        let previous_header = prev_pending_blocks.latest_header();
        let current_block = BlockAssembler::assemble(std::slice::from_ref(flashblock))?;
        let l1_block_info = current_block.l1_block_info()?;
        let AssembledBlock { block: assembled_block, header: assembled_header, .. } = current_block;
        let pending_block = Block {
            header: Header {
                parent_hash: base.parent_hash,
                number: base.block_number,
                timestamp: base.timestamp,
                gas_limit: base.gas_limit,
                base_fee_per_gas: Some(base.base_fee_per_gas.saturating_to()),
                ..Default::default()
            },
            body: assembled_block.body,
        };

        db.block_hashes.insert(base.block_number - 1, base.parent_hash);

        let evm_config = BaseEvmConfig::base(self.client.chain_spec());
        let block_env_attributes = BaseNextBlockEnvAttributes {
            timestamp: base.timestamp,
            suggested_fee_recipient: base.fee_recipient,
            prev_randao: base.prev_randao,
            gas_limit: base.gas_limit,
            parent_beacon_block_root: Some(base.parent_beacon_block_root),
            extra_data: base.extra_data.clone(),
        };
        let evm_env = evm_config
            .next_evm_env(&previous_header, &block_env_attributes)
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
        let evm = evm_config.evm_with_env(db, evm_env);

        let recovery_start = Instant::now();
        let txs_with_senders: Vec<(BaseTxEnvelope, Address)> = pending_block
            .body
            .transactions
            .par_iter()
            .cloned()
            .map(|tx| -> Result<(BaseTxEnvelope, Address)> {
                let sender = tx.recover_signer()?;
                Ok((tx, sender))
            })
            .collect::<Result<_>>()?;
        Metrics::sender_recovery_duration().record(recovery_start.elapsed());

        let mut pending_blocks_builder = PendingBlocksBuilder::from_previous(prev_pending_blocks);
        pending_blocks_builder.with_flashblocks([flashblock.clone()]);
        pending_blocks_builder.with_header(assembled_header);

        let mut pending_state_builder = PendingStateBuilder::new(
            self.client.chain_spec(),
            evm,
            pending_block,
            None,
            l1_block_info.clone(),
            state_overrides,
        );
        pending_state_builder
            .apply_pre_execution_changes(base.parent_hash, Some(base.parent_beacon_block_root))?;

        for (idx, (transaction, sender)) in txs_with_senders.into_iter().enumerate() {
            let tx_hash = transaction.tx_hash();

            pending_blocks_builder.with_transaction_sender(tx_hash, sender);
            pending_blocks_builder.increment_nonce(sender);

            let recovered_transaction = Recovered::new_unchecked(transaction, sender);
            let executed_transaction =
                pending_state_builder.execute_transaction(idx, recovered_transaction)?;

            if let Some(time_us) = executed_transaction.execution_time_us {
                pending_blocks_builder.with_execution_time(tx_hash, time_us);
            }

            for (address, account) in &executed_transaction.state {
                if account.is_touched() {
                    pending_blocks_builder.with_account_balance(*address, account.info.balance);
                }
            }

            pending_blocks_builder.with_transaction(executed_transaction.rpc_transaction);
            pending_blocks_builder.with_receipt(tx_hash, executed_transaction.receipt);
            pending_blocks_builder.with_transaction_state(tx_hash, executed_transaction.state);
            pending_blocks_builder.with_transaction_result(tx_hash, executed_transaction.result);
        }

        let latest_block_cumulative_gas_used = pending_state_builder.cumulative_gas_used();
        let latest_block_next_log_index = pending_state_builder.next_log_index();
        let (db, state_overrides) = pending_state_builder.into_db_and_state_overrides();
        pending_blocks_builder.with_latest_block_context(
            prev_pending_blocks.pending_transaction_count(),
            base,
            l1_block_info,
            flashblock.diff.transactions.len(),
            latest_block_cumulative_gas_used,
            latest_block_next_log_index,
        );

        self.publish_pending_blocks(pending_blocks_builder, db, state_overrides)
    }

    #[instrument(level = "debug", skip_all, fields(num_flashblocks = flashblocks.len()))]
    fn build_pending_state(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblocks: &[Flashblock],
    ) -> Result<Option<Arc<PendingBlocks>>> {
        // BTreeMap guarantees ascending order of keys while iterating
        let mut flashblocks_per_block = BTreeMap::<BlockNumber, Vec<Flashblock>>::new();
        for flashblock in flashblocks {
            flashblocks_per_block
                .entry(flashblock.metadata.block_number)
                .or_default()
                .push(flashblock.clone());
        }

        let earliest_block_number =
            *flashblocks_per_block.keys().min().ok_or(ProtocolError::EmptyFlashblocks)?;
        let canonical_block =
            earliest_block_number.checked_sub(1).ok_or(ProtocolError::ZeroBlockNumber)?;
        let mut last_block_header = self
            .client
            .header_by_number(canonical_block)
            .map_err(|e| ProviderError::StateProvider(e.to_string()))?
            .ok_or(ProviderError::MissingCanonicalHeader { block_number: canonical_block })?;
        let mut last_block_hash = last_block_header.hash_slow();

        let evm_config = BaseEvmConfig::base(self.client.chain_spec());
        let state_provider = self
            .client
            .state_by_block_number_or_tag(BlockNumberOrTag::Number(canonical_block))
            .map_err(|e| ProviderError::StateProvider(e.to_string()))?;
        let state_provider_db = StateProviderDatabase::new(state_provider);
        let mut pending_blocks_builder = PendingBlocksBuilder::new();

        // Track state changes across flashblocks, accumulating bundle state
        // from previous pending blocks if available.
        let mut db = State::builder().with_database(state_provider_db).with_bundle_update().build();

        let mut state_overrides =
            prev_pending_blocks.as_ref().map_or_else(StateOverride::default, |pending_blocks| {
                pending_blocks.get_state_overrides().unwrap_or_default()
            });

        let mut total_transaction_count = 0usize;
        for (_block_number, flashblocks) in flashblocks_per_block {
            // Use BlockAssembler to reconstruct the block from flashblocks
            let assembled = BlockAssembler::assemble(&flashblocks)?;
            if assembled.base.parent_hash != last_block_hash {
                return Err(ProtocolError::ParentHashMismatch {
                    expected: last_block_hash,
                    actual: assembled.base.parent_hash,
                }
                .into());
            }
            let latest_flashblock_tx_count =
                flashblocks.last().map(|latest| latest.diff.transactions.len()).unwrap_or_default();
            let latest_block_hash =
                flashblocks.last().map(|latest| latest.diff.block_hash).unwrap_or_default();
            let latest_block_base = assembled.base.clone();

            pending_blocks_builder.with_flashblocks(assembled.flashblocks.clone());
            pending_blocks_builder.with_header(assembled.header.clone());

            // Extract L1 block info using the AssembledBlock method
            let l1_block_info = assembled.l1_block_info()?;
            let latest_block_l1_block_info = l1_block_info.clone();
            let latest_block_transaction_count = assembled.block.body.transactions.len();

            let block_env_attributes = BaseNextBlockEnvAttributes {
                timestamp: assembled.base.timestamp,
                suggested_fee_recipient: assembled.base.fee_recipient,
                prev_randao: assembled.base.prev_randao,
                gas_limit: assembled.base.gas_limit,
                parent_beacon_block_root: Some(assembled.base.parent_beacon_block_root),
                extra_data: assembled.base.extra_data.clone(),
            };

            db.block_hashes
                .insert(latest_block_base.block_number - 1, latest_block_base.parent_hash);

            let evm_env = evm_config
                .next_evm_env(&last_block_header, &block_env_attributes)
                .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
            let evm = evm_config.evm_with_env(db, evm_env);

            // Parallel sender recovery - batch all ECDSA operations upfront
            let recovery_start = Instant::now();
            let txs_with_senders: Vec<(BaseTxEnvelope, Address)> = assembled
                .block
                .body
                .transactions
                .par_iter()
                .cloned()
                .map(|tx| -> Result<(BaseTxEnvelope, Address)> {
                    let tx_hash = tx.tx_hash();
                    let sender = match prev_pending_blocks
                        .as_ref()
                        .and_then(|p| p.get_transaction_sender(&tx_hash))
                    {
                        Some(cached) => cached,
                        None => tx.recover_signer()?,
                    };
                    Ok((tx, sender))
                })
                .collect::<Result<_>>()?;
            Metrics::sender_recovery_duration().record(recovery_start.elapsed());

            // Clone header before moving block to avoid cloning the entire block
            let block_header = assembled.block.header.clone();

            let parent_block_hash = assembled.base.parent_hash;
            let parent_beacon_block_root = Some(assembled.base.parent_beacon_block_root);

            let mut pending_state_builder = PendingStateBuilder::new(
                self.client.chain_spec(),
                evm,
                assembled.block,
                prev_pending_blocks.clone(),
                l1_block_info,
                state_overrides,
            );

            pending_state_builder
                .apply_pre_execution_changes(parent_block_hash, parent_beacon_block_root)?;

            for (idx, (transaction, sender)) in txs_with_senders.into_iter().enumerate() {
                let tx_hash = transaction.tx_hash();

                pending_blocks_builder.with_transaction_sender(tx_hash, sender);
                pending_blocks_builder.increment_nonce(sender);

                let recovered_transaction = Recovered::new_unchecked(transaction, sender);

                let executed_transaction =
                    pending_state_builder.execute_transaction(idx, recovered_transaction)?;

                if let Some(time_us) = executed_transaction.execution_time_us {
                    pending_blocks_builder.with_execution_time(tx_hash, time_us);
                }

                for (address, account) in &executed_transaction.state {
                    if account.is_touched() {
                        pending_blocks_builder.with_account_balance(*address, account.info.balance);
                    }
                }

                pending_blocks_builder.with_transaction(executed_transaction.rpc_transaction);
                pending_blocks_builder.with_receipt(tx_hash, executed_transaction.receipt);
                pending_blocks_builder.with_transaction_state(tx_hash, executed_transaction.state);
                pending_blocks_builder
                    .with_transaction_result(tx_hash, executed_transaction.result);
            }

            let latest_flashblock_tx_start = total_transaction_count
                .saturating_add(latest_block_transaction_count)
                .saturating_sub(latest_flashblock_tx_count);
            pending_blocks_builder.with_latest_block_context(
                latest_flashblock_tx_start,
                latest_block_base,
                latest_block_l1_block_info,
                latest_block_transaction_count,
                pending_state_builder.cumulative_gas_used(),
                pending_state_builder.next_log_index(),
            );
            total_transaction_count += latest_block_transaction_count;

            (db, state_overrides) = pending_state_builder.into_db_and_state_overrides();
            last_block_header = block_header;
            last_block_hash = latest_block_hash;
        }

        self.publish_pending_blocks(pending_blocks_builder, db, state_overrides)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};
    use alloy_rpc_types_engine::PayloadId;
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Metadata,
    };
    use reth_primitives_traits::SealedBlock;

    use super::*;

    fn flashblock(block_number: u64, index: u64, parent_hash: B256) -> Flashblock {
        Flashblock {
            payload_id: PayloadId::default(),
            index,
            base: (index == 0).then_some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number,
                gas_limit: 30_000_000,
                timestamp: 1_700_000_000,
                extra_data: Default::default(),
                base_fee_per_gas: Default::default(),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1::default(),
            metadata: Metadata::new(block_number),
        }
    }

    #[test]
    fn flashblock_anchor_is_parent_block() {
        let update = StateUpdate::Flashblock(flashblock(101, 0, B256::ZERO));
        assert_eq!(update.pending_anchor_number(), 100);
        assert_eq!(
            StateUpdate::Flashblock(flashblock(0, 0, B256::ZERO)).pending_anchor_number(),
            0
        );
    }

    #[test]
    fn recovery_resume_requires_current_index_zero_and_parent_hash() {
        let best = 100;
        let best_hash = B256::repeat_byte(0xAB);
        let resume = StateUpdate::Flashblock(flashblock(101, 0, best_hash));

        assert!(resume.is_recovery_resume(best, best_hash));
        assert!(!resume.is_recovery_resume(best, B256::repeat_byte(0xCD)));
        assert!(
            !StateUpdate::Flashblock(flashblock(101, 1, best_hash))
                .is_recovery_resume(best, best_hash)
        );
        assert!(
            !StateUpdate::Flashblock(flashblock(102, 0, best_hash))
                .is_recovery_resume(best, best_hash)
        );
    }

    #[test]
    fn preflight_restarts_from_current_base_when_pending_is_stale() {
        let best = 100;
        let best_hash = B256::repeat_byte(0xAB);
        let update = StateUpdate::Flashblock(flashblock(101, 0, best_hash));

        assert_eq!(
            update.preflight((best, best_hash), true, false, false),
            UpdatePreflight::ResumeRecovery
        );
    }

    #[test]
    fn recovery_only_resumes_from_matching_tip_child() {
        let best = 100;
        let best_hash = B256::repeat_byte(0xAB);
        let wrong_parent = StateUpdate::Flashblock(flashblock(101, 0, B256::repeat_byte(0xCD)));
        let resume = StateUpdate::Flashblock(flashblock(101, 0, best_hash));

        assert_eq!(
            wrong_parent.preflight((best, best_hash), false, false, true),
            UpdatePreflight::Skip
        );
        assert_eq!(
            resume.preflight((best, best_hash), false, false, true),
            UpdatePreflight::ResumeRecovery
        );
    }

    #[test]
    fn recovery_admits_future_flashblocks_for_bounded_caching() {
        let best = 100;
        let best_hash = B256::repeat_byte(0xAB);
        let future = StateUpdate::Flashblock(flashblock(102, 0, B256::repeat_byte(0xCD)));

        assert_eq!(
            future.preflight((best, best_hash), false, false, true),
            UpdatePreflight::Process
        );
    }

    #[test]
    fn recovery_processes_current_canonical_for_cache_replay() {
        let best = 100;
        let block = BaseBlock {
            header: Header { number: best, ..Default::default() },
            body: Default::default(),
        };
        let block = RecoveredBlock::new_sealed(SealedBlock::seal_slow(block), Vec::new());
        let best_hash = block.hash();

        assert_eq!(
            StateUpdate::Canonical(block).preflight((best, best_hash), false, false, true),
            UpdatePreflight::Process
        );
    }

    #[test]
    fn preflight_enters_recovery_for_canonical_ahead_of_provider_visibility() {
        let best = 100;
        let best_hash = B256::repeat_byte(0xAB);
        let block = BaseBlock {
            header: Header { number: best + 1, ..Default::default() },
            body: Default::default(),
        };
        let block = RecoveredBlock::new_sealed(SealedBlock::seal_slow(block), Vec::new());

        assert_eq!(
            StateUpdate::Canonical(block).preflight((best, best_hash), true, true, false),
            UpdatePreflight::EnterRecovery
        );
    }

    #[test]
    fn preflight_drops_late_work_without_clearing_fresh_pending() {
        let best = 100;
        let best_hash = B256::repeat_byte(0xAB);
        let stale = StateUpdate::Flashblock(flashblock(100, 0, B256::repeat_byte(0xCD)));

        assert_eq!(stale.preflight((best, best_hash), true, true, false), UpdatePreflight::Skip);
    }
}
