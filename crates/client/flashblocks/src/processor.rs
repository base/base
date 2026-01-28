//! Flashblocks state processor.

use std::{collections::BTreeMap, ops::Range, sync::Arc, time::Instant};

use alloy_consensus::{
    Header,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::{BlockNumberOrTag, eip7928::BlockAccessList};
use alloy_primitives::{Address, BlockNumber, StorageKey};
use alloy_rpc_types_eth::state::StateOverride;
use arc_swap::ArcSwapOption;
use base_flashtypes::Flashblock;
use futures_util::StreamExt;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_network::TransactionResponse;
use rayon::prelude::*;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_engine_tree::tree::{CachedStateMetrics, CachedStateProvider, ExecutionCache};
use reth_evm::ConfigureEvm;
use reth_optimism_chainspec::OpHardforks;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_primitives::OpBlock;
use reth_primitives::RecoveredBlock;
use reth_provider::{AccountReader, BlockReaderIdExt, StateProvider, StateProviderFactory};
use reth_revm::{State, database::StateProviderDatabase};
use revm_database::states::bundle_state::BundleRetention;
use tokio::sync::{Mutex, broadcast::Sender, mpsc::UnboundedReceiver};

use crate::{
    BlockAssembler, ExecutionError, Metrics, PendingBlocks, PendingBlocksBuilder,
    PendingStateBuilder, ProviderError, Result,
    validation::{
        CanonicalBlockReconciler, FlashblockSequenceValidator, ReconciliationStrategy,
        ReorgDetector, SequenceValidationResult,
    },
};

/// Default cache size in bytes (100MB).
const DEFAULT_CACHE_SIZE: usize = 100_000_000;

/// Default maximum number of prewarm workers.
const DEFAULT_MAX_PREWARM_WORKERS: usize = 8;

/// Returns the total number of storage slots in a BlockAccessList.
fn total_slots(bal: &BlockAccessList) -> usize {
    bal.iter()
        .map(|account| account.storage_changes.len() + account.storage_reads.len())
        .sum()
}

/// Iterator over storage slots in a BlockAccessList with range-based filtering.
struct BALSlotIter<'a> {
    bal: &'a BlockAccessList,
    range: Range<usize>,
    current_index: usize,
    account_idx: usize,
    slot_idx: usize,
}

impl<'a> BALSlotIter<'a> {
    fn new(bal: &'a BlockAccessList, range: Range<usize>) -> Self {
        let mut iter = Self { bal, range, current_index: 0, account_idx: 0, slot_idx: 0 };
        iter.skip_to_range_start();
        iter
    }

    fn skip_to_range_start(&mut self) {
        while self.account_idx < self.bal.len() {
            let account = &self.bal[self.account_idx];
            let slots_in_account = account.storage_changes.len() + account.storage_reads.len();
            let account_end = self.current_index + slots_in_account;

            if account_end <= self.range.start {
                self.current_index = account_end;
                self.account_idx += 1;
                self.slot_idx = 0;
            } else if self.current_index < self.range.start {
                let skip_slots = self.range.start - self.current_index;
                self.slot_idx = skip_slots;
                self.current_index = self.range.start;
                break;
            } else {
                break;
            }
        }
    }
}

impl<'a> Iterator for BALSlotIter<'a> {
    type Item = (Address, StorageKey);

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_index >= self.range.end {
            return None;
        }

        while self.account_idx < self.bal.len() {
            let account = &self.bal[self.account_idx];
            let changed_len = account.storage_changes.len();
            let total_len = changed_len + account.storage_reads.len();

            if self.slot_idx < total_len {
                let address = account.address;
                let slot = if self.slot_idx < changed_len {
                    account.storage_changes[self.slot_idx].slot
                } else {
                    account.storage_reads[self.slot_idx - changed_len]
                };

                self.slot_idx += 1;
                self.current_index += 1;

                if self.current_index > self.range.end {
                    return None;
                }

                return Some((address, StorageKey::from(slot)));
            }

            self.account_idx += 1;
            self.slot_idx = 0;
        }

        None
    }
}

/// Prefetches storage slots from a BAL into the cache using parallel workers.
///
/// This spawns multiple rayon workers that iterate through assigned ranges of the BAL
/// and access each storage slot to populate the cache. Each worker creates its own
/// state provider from the client for thread safety.
fn prefetch_bal_slots<Client>(
    bal: &BlockAccessList,
    client: &Client,
    block_number: BlockNumber,
    cache: &ExecutionCache,
    metrics: &CachedStateMetrics,
    max_workers: usize,
) where
    Client: StateProviderFactory + Sync,
{
    let total = total_slots(bal);
    if total == 0 {
        trace!(message = "no slots to prefetch in BAL");
        return;
    }

    let workers_needed = total.min(max_workers);
    let slots_per_worker = total / workers_needed;
    let remainder = total % workers_needed;

    debug!(
        message = "starting BAL prefetch",
        total_slots = total,
        workers = workers_needed,
        slots_per_worker = slots_per_worker
    );

    // Use rayon to spawn parallel workers
    (0..workers_needed).into_par_iter().for_each(|i| {
        let start = i * slots_per_worker + i.min(remainder);
        let extra = if i < remainder { 1 } else { 0 };
        let end = start + slots_per_worker + extra;

        // Each worker creates its own state provider for thread safety
        let state_provider = match client.state_by_block_number_or_tag(BlockNumberOrTag::Number(block_number)) {
            Ok(provider) => provider,
            Err(err) => {
                trace!(
                    message = "failed to create state provider for prewarm worker",
                    worker = i,
                    error = %err
                );
                return;
            }
        };

        let cached_provider =
            CachedStateProvider::new(state_provider, cache.clone(), metrics.clone());

        let mut last_address = None;
        let mut slots_fetched = 0;

        for (address, slot) in BALSlotIter::new(bal, start..end) {
            // Fetch account if this is a new address
            if last_address != Some(address) {
                let _ = cached_provider.basic_account(&address);
                last_address = Some(address);
            }
            // Access the slot to populate the cache
            let _ = cached_provider.storage(address, slot);
            slots_fetched += 1;
        }

        trace!(
            message = "prewarm worker completed",
            worker = i,
            slots_fetched = slots_fetched
        );
    });
}

/// Messages consumed by the state processor.
#[derive(Debug, Clone)]
pub enum StateUpdate {
    /// New canonical block to reconcile against pending state.
    Canonical(RecoveredBlock<OpBlock>),
    /// Incoming flashblock payload to extend pending state.
    Flashblock(Flashblock),
}

/// Processes flashblocks and canonical blocks to keep pending state updated.
#[derive(Debug, Clone)]
pub struct StateProcessor<Client> {
    rx: Arc<Mutex<UnboundedReceiver<StateUpdate>>>,
    pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
    max_depth: u64,
    metrics: Metrics,
    client: Client,
    sender: Sender<Arc<PendingBlocks>>,
}

impl<Client> StateProcessor<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + OpHardforks>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + 'static,
{
    /// Creates a new state processor wired to the provided channels and state.
    pub fn new(
        client: Client,
        pending_blocks: Arc<ArcSwapOption<PendingBlocks>>,
        max_depth: u64,
        rx: Arc<Mutex<UnboundedReceiver<StateUpdate>>>,
        sender: Sender<Arc<PendingBlocks>>,
    ) -> Self {
        Self { metrics: Metrics::default(), pending_blocks, client, max_depth, rx, sender }
    }

    /// Processes updates from the queue until the channel closes.
    pub async fn start(&self) {
        while let Some(update) = self.rx.lock().await.recv().await {
            let prev_pending_blocks = self.pending_blocks.load_full();
            match update {
                StateUpdate::Canonical(block) => {
                    debug!(message = "processing canonical block", block_number = block.number);
                    match self.process_canonical_block(prev_pending_blocks, &block).await {
                        Ok(new_pending_blocks) => {
                            self.pending_blocks.swap(new_pending_blocks);
                        }
                        Err(e) => {
                            error!(message = "could not process canonical block", error = %e);
                        }
                    }
                }
                StateUpdate::Flashblock(flashblock) => {
                    let start_time = Instant::now();
                    debug!(
                        message = "processing flashblock",
                        block_number = flashblock.metadata.block_number,
                        flashblock_index = flashblock.index
                    );
                    match self.process_flashblock(prev_pending_blocks, flashblock).await {
                        Ok(new_pending_blocks) => {
                            if new_pending_blocks.is_some() {
                                _ = self.sender.send(new_pending_blocks.clone().unwrap())
                            }

                            self.pending_blocks.swap(new_pending_blocks);
                            self.metrics.block_processing_duration.record(start_time.elapsed());
                        }
                        Err(e) => {
                            error!(message = "could not process Flashblock", error = %e);
                            self.metrics.block_processing_error.increment(1);
                        }
                    }
                }
            }
        }
    }

    async fn process_canonical_block(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        block: &RecoveredBlock<OpBlock>,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        let pending_blocks = match &prev_pending_blocks {
            Some(pb) => pb,
            None => {
                debug!(message = "no pending state to update with canonical block, skipping");
                return Ok(None);
            }
        };

        let mut flashblocks = pending_blocks.get_flashblocks();
        let num_flashblocks_for_canon =
            flashblocks.iter().filter(|fb| fb.metadata.block_number == block.number).count();
        self.metrics.flashblocks_in_block.record(num_flashblocks_for_canon as f64);
        self.metrics.pending_snapshot_height.set(pending_blocks.latest_block_number() as f64);

        // Check for reorg by comparing transaction sets
        let tracked_txns = pending_blocks.get_transactions_for_block(block.number);
        let tracked_txn_hashes: Vec<_> = tracked_txns.iter().map(|tx| tx.tx_hash()).collect();
        let block_txn_hashes: Vec<_> = block.body().transactions().map(|tx| tx.tx_hash()).collect();

        let reorg_result = ReorgDetector::detect(&tracked_txn_hashes, &block_txn_hashes);
        let reorg_detected = reorg_result.is_reorg();

        // Determine the reconciliation strategy
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(pending_blocks.earliest_block_number()),
            Some(pending_blocks.latest_block_number()),
            block.number,
            self.max_depth,
            reorg_detected,
        );

        match strategy {
            ReconciliationStrategy::CatchUp => {
                debug!(
                    message = "pending snapshot cleared because canonical caught up",
                    latest_pending_block = pending_blocks.latest_block_number(),
                    canonical_block = block.number,
                );
                self.metrics.pending_clear_catchup.increment(1);
                self.metrics
                    .pending_snapshot_fb_index
                    .set(pending_blocks.latest_flashblock_index() as f64);
                Ok(None)
            }
            ReconciliationStrategy::HandleReorg => {
                warn!(
                    message = "reorg detected, recomputing pending flashblocks going ahead of reorg",
                    tracked_txn_hashes = ?tracked_txn_hashes,
                    block_txn_hashes = ?block_txn_hashes,
                );
                self.metrics.pending_clear_reorg.increment(1);

                // If there is a reorg, we re-process all future flashblocks without reusing the existing pending state
                flashblocks.retain(|flashblock| flashblock.metadata.block_number > block.number);
                self.build_pending_state(None, &flashblocks).await
            }
            ReconciliationStrategy::DepthLimitExceeded { depth, max_depth } => {
                debug!(
                    message = "pending blocks depth exceeds max depth, resetting pending blocks",
                    pending_blocks_depth = depth,
                    max_depth = max_depth,
                );

                flashblocks.retain(|flashblock| flashblock.metadata.block_number > block.number);
                self.build_pending_state(None, &flashblocks).await
            }
            ReconciliationStrategy::Continue => {
                debug!(
                    message = "canonical block behind latest pending block, continuing with existing pending state",
                    latest_pending_block = pending_blocks.latest_block_number(),
                    earliest_pending_block = pending_blocks.earliest_block_number(),
                    canonical_block = block.number,
                    pending_txns_for_block = ?tracked_txn_hashes.len(),
                    canonical_txns_for_block = ?block_txn_hashes.len(),
                );
                // If no reorg, we can continue building on top of the existing pending state
                // NOTE: We do not retain specific flashblocks here to avoid losing track of our "earliest" pending block number
                self.build_pending_state(prev_pending_blocks, &flashblocks).await
            }
            ReconciliationStrategy::NoPendingState => {
                // This case is already handled above, but included for completeness
                debug!(message = "no pending state to update with canonical block, skipping");
                Ok(None)
            }
        }
    }

    async fn process_flashblock(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblock: Flashblock,
    ) -> Result<Option<Arc<PendingBlocks>>> {
        let pending_blocks = match &prev_pending_blocks {
            Some(pb) => pb,
            None => {
                if flashblock.index == 0 {
                    return self.build_pending_state(None, &[flashblock]).await;
                } else {
                    info!(message = "waiting for first Flashblock");
                    return Ok(None);
                }
            }
        };

        let validation_result = FlashblockSequenceValidator::validate(
            pending_blocks.latest_block_number(),
            pending_blocks.latest_flashblock_index(),
            flashblock.metadata.block_number,
            flashblock.index,
        );

        match validation_result {
            SequenceValidationResult::NextInSequence
            | SequenceValidationResult::FirstOfNextBlock => {
                // We have received the next flashblock for the current block
                // or the first flashblock for the next block
                let mut flashblocks = pending_blocks.get_flashblocks();
                flashblocks.push(flashblock);
                self.build_pending_state(prev_pending_blocks, &flashblocks).await
            }
            SequenceValidationResult::Duplicate => {
                // We have received a duplicate flashblock for the current block
                self.metrics.unexpected_block_order.increment(1);
                warn!(
                    message = "Received duplicate Flashblock for current block, ignoring",
                    curr_block = %pending_blocks.latest_block_number(),
                    flashblock_index = %flashblock.index,
                );
                Ok(prev_pending_blocks)
            }
            SequenceValidationResult::InvalidNewBlockIndex { block_number, index: _ } => {
                // We have received a non-zero flashblock for a new block
                self.metrics.unexpected_block_order.increment(1);
                error!(
                    message = "Received non-zero index Flashblock for new block, zeroing Flashblocks until we receive a base Flashblock",
                    curr_block = %pending_blocks.latest_block_number(),
                    new_block = %block_number,
                );
                Ok(None)
            }
            SequenceValidationResult::NonSequentialGap { expected: _, actual: _ } => {
                // We have received a non-sequential Flashblock for the current block
                self.metrics.unexpected_block_order.increment(1);
                error!(
                    message = "Received non-sequential Flashblock for current block, zeroing Flashblocks until we receive a base Flashblock",
                    curr_block = %pending_blocks.latest_block_number(),
                    new_block = %flashblock.metadata.block_number,
                );
                Ok(None)
            }
        }
    }

    async fn build_pending_state(
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

        let earliest_block_number = flashblocks_per_block.keys().min().unwrap();
        let canonical_block = earliest_block_number - 1;
        let mut last_block_header = self
            .client
            .header_by_number(canonical_block)
            .map_err(|e| ProviderError::StateProvider(e.to_string()))?
            .ok_or(ProviderError::MissingCanonicalHeader { block_number: canonical_block })?;

        let evm_config = OpEvmConfig::optimism(self.client.chain_spec());
        let state_provider = self
            .client
            .state_by_block_number_or_tag(BlockNumberOrTag::Number(canonical_block))
            .map_err(|e| ProviderError::StateProvider(e.to_string()))?;

        // TODO: Get access list from flashblock metadata
        let access_list: Option<BlockAccessList> = None;

        // Create execution cache for state prewarming
        let cache = ExecutionCache::new(DEFAULT_CACHE_SIZE);
        let cache_metrics = CachedStateMetrics::zeroed();

        // If we have a BAL, prefetch storage slots into the cache
        if let Some(ref bal) = access_list {
            let prewarm_start = Instant::now();
            let slots = total_slots(bal);
            debug!(
                message = "starting BAL prewarm",
                block_number = canonical_block + 1,
                total_slots = slots
            );

            prefetch_bal_slots(
                bal,
                &self.client,
                canonical_block,
                &cache,
                &cache_metrics,
                DEFAULT_MAX_PREWARM_WORKERS,
            );

            debug!(
                message = "BAL prewarm complete",
                block_number = canonical_block + 1,
                duration_ms = prewarm_start.elapsed().as_millis() as u64
            );
        } else {
            trace!(message = "no BAL available for prewarming", block_number = canonical_block + 1);
        }

        // Wrap state provider with cache for execution
        let cached_state_provider =
            CachedStateProvider::new(state_provider, cache, cache_metrics);
        let state_provider_db = StateProviderDatabase::new(cached_state_provider);
        let mut pending_blocks_builder = PendingBlocksBuilder::new();

        // Track state changes across flashblocks, accumulating bundle state
        // from previous pending blocks if available.
        let mut db = match &prev_pending_blocks {
            Some(pending_blocks) => State::builder()
                .with_database(state_provider_db)
                .with_bundle_update()
                .with_bundle_prestate(pending_blocks.get_bundle_state())
                .build(),
            None => State::builder().with_database(state_provider_db).with_bundle_update().build(),
        };

        let mut state_overrides =
            prev_pending_blocks.as_ref().map_or_else(StateOverride::default, |pending_blocks| {
                pending_blocks.get_state_overrides().unwrap_or_default()
            });

        for (_block_number, flashblocks) in flashblocks_per_block {
            // Use BlockAssembler to reconstruct the block from flashblocks
            let assembled = BlockAssembler::assemble(&flashblocks)?;

            pending_blocks_builder.with_flashblocks(assembled.flashblocks.clone());
            pending_blocks_builder.with_header(assembled.header.clone());

            // Extract L1 block info using the AssembledBlock method
            let l1_block_info = assembled.l1_block_info()?;

            let block_env_attributes = OpNextBlockEnvAttributes {
                timestamp: assembled.base.timestamp,
                suggested_fee_recipient: assembled.base.fee_recipient,
                prev_randao: assembled.base.prev_randao,
                gas_limit: assembled.base.gas_limit,
                parent_beacon_block_root: Some(assembled.base.parent_beacon_block_root),
                extra_data: assembled.base.extra_data.clone(),
            };

            let evm_env = evm_config
                .next_evm_env(&last_block_header, &block_env_attributes)
                .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
            let evm = evm_config.evm_with_env(db, evm_env);

            // Parallel sender recovery - batch all ECDSA operations upfront
            let recovery_start = Instant::now();
            let txs_with_senders: Vec<(OpTxEnvelope, Address)> = assembled
                .block
                .body
                .transactions
                .par_iter()
                .cloned()
                .map(|tx| -> Result<(OpTxEnvelope, Address)> {
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
            self.metrics.sender_recovery_duration.record(recovery_start.elapsed());

            // Clone header before moving block to avoid cloning the entire block
            let block_header = assembled.block.header.clone();

            // Prepare the batch of recovered transactions for execution
            let recovered_transactions: Vec<Recovered<OpTxEnvelope>> = txs_with_senders
                .iter()
                .map(|(tx, sender)| Recovered::new_unchecked(tx.clone(), *sender))
                .collect();

            // Register transaction senders with the pending blocks builder
            for (tx, sender) in &txs_with_senders {
                pending_blocks_builder.with_transaction_sender(tx.tx_hash(), *sender);
                pending_blocks_builder.increment_nonce(*sender);
            }

            let pending_state_builder = PendingStateBuilder::new(
                self.client.chain_spec(),
                evm,
                assembled.block,
                prev_pending_blocks.clone(),
                l1_block_info,
                state_overrides,
            );

            // Execute the batch and consume results from the async stream
            let batch_result = pending_state_builder.execute_batch(recovered_transactions);
            let (mut result_stream, execution_handle) = batch_result.into_stream();

            while let Some(result) = result_stream.next().await {
                let executed_transaction = result?;
                let tx_hash = executed_transaction.rpc_transaction.inner.inner.tx_hash();

                for (address, account) in executed_transaction.state.iter() {
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

            (db, state_overrides) = execution_handle.into_db_and_state_overrides();
            last_block_header = block_header;
        }

        // Extract the accumulated bundle state for state root calculation
        db.merge_transitions(BundleRetention::Reverts);
        pending_blocks_builder.with_bundle_state(db.take_bundle());
        pending_blocks_builder.with_state_overrides(state_overrides);

        Ok(Some(Arc::new(pending_blocks_builder.build()?)))
    }
}
