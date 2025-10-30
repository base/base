//! Flashblocks state processor.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::Instant,
};

use alloy_consensus::{
    Header, TxReceipt,
    transaction::{Recovered, SignerRecoverable, TransactionMeta},
};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, BlockNumber, Bytes, Sealable, map::foldhash::HashMap};
use alloy_rpc_types::{TransactionTrait, Withdrawal};
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use alloy_rpc_types_eth::state::StateOverride;
use arc_swap::ArcSwapOption;
use base_flashtypes::Flashblock;
use eyre::eyre;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_network::TransactionResponse;
use op_alloy_rpc_types::Transaction;
use reth::{
    chainspec::{ChainSpecProvider, EthChainSpec},
    providers::{BlockReaderIdExt, StateProviderFactory},
    revm::{
        context::result::ResultAndState,
        database::StateProviderDatabase,
        db::CacheDB,
        DatabaseCommit,
        State,
    },
};
use revm_database::states::bundle_state::BundleRetention;
use reth_evm::{ConfigureEvm, Evm};
use reth_optimism_chainspec::OpHardforks;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_primitives::{DepositReceipt, OpBlock, OpPrimitives};
use reth_optimism_rpc::OpReceiptBuilder;
use reth_primitives::RecoveredBlock;
use reth_rpc_convert::transaction::ConvertReceiptInput;
use tokio::sync::{Mutex, broadcast::Sender, mpsc::UnboundedReceiver};
use tracing::{debug, error, info, warn};

use crate::{Metrics, PendingBlocks, PendingBlocksBuilder};

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
                    match self.process_canonical_block(prev_pending_blocks, &block) {
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
                    match self.process_flashblock(prev_pending_blocks, flashblock) {
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

    fn process_canonical_block(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        block: &RecoveredBlock<OpBlock>,
    ) -> eyre::Result<Option<Arc<PendingBlocks>>> {
        match &prev_pending_blocks {
            Some(pending_blocks) => {
                let mut flashblocks = pending_blocks.get_flashblocks();
                let num_flashblocks_for_canon = flashblocks
                    .iter()
                    .filter(|fb| fb.metadata.block_number == block.number)
                    .count();
                self.metrics.flashblocks_in_block.record(num_flashblocks_for_canon as f64);
                self.metrics
                    .pending_snapshot_height
                    .set(pending_blocks.latest_block_number() as f64);

                if pending_blocks.latest_block_number() <= block.number {
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
                } else {
                    // If we had a reorg, we need to reset all flashblocks state
                    let tracked_txns = pending_blocks.get_transactions_for_block(block.number);
                    let tracked_txn_hashes: HashSet<_> =
                        tracked_txns.iter().map(|tx| tx.tx_hash()).collect();
                    let block_txn_hashes: HashSet<_> =
                        block.body().transactions().map(|tx| tx.tx_hash()).collect();
                    let pending_blocks_depth =
                        block.number - pending_blocks.earliest_block_number();

                    debug!(
                        message = "canonical block behind latest pending block, checking for reorg and max depth",
                        latest_pending_block = pending_blocks.latest_block_number(),
                        earliest_pending_block = pending_blocks.earliest_block_number(),
                        canonical_block = block.number,
                        pending_txns_for_block = ?tracked_txn_hashes.len(),
                        canonical_txns_for_block = ?block_txn_hashes.len(),
                        pending_blocks_depth = pending_blocks_depth,
                        max_depth = self.max_depth,
                    );

                    if tracked_txn_hashes.len() != block_txn_hashes.len()
                        || tracked_txn_hashes != block_txn_hashes
                    {
                        debug!(
                            message = "reorg detected, recomputing pending flashblocks going ahead of reorg",
                            tracked_txn_hashes = ?tracked_txn_hashes,
                            block_txn_hashes = ?block_txn_hashes,
                        );
                        self.metrics.pending_clear_reorg.increment(1);

                        // If there is a reorg, we re-process all future flashblocks without reusing the existing pending state
                        flashblocks
                            .retain(|flashblock| flashblock.metadata.block_number > block.number);
                        return self.build_pending_state(None, &flashblocks);
                    }

                    if pending_blocks_depth > self.max_depth {
                        debug!(
                            message =
                                "pending blocks depth exceeds max depth, resetting pending blocks",
                            pending_blocks_depth = pending_blocks_depth,
                            max_depth = self.max_depth,
                        );

                        flashblocks
                            .retain(|flashblock| flashblock.metadata.block_number > block.number);
                        return self.build_pending_state(None, &flashblocks);
                    }

                    // If no reorg, we can continue building on top of the existing pending state
                    // NOTE: We do not retain specific flashblocks here to avoid losing track of our "earliest" pending block number
                    self.build_pending_state(prev_pending_blocks, &flashblocks)
                }
            }
            None => {
                debug!(message = "no pending state to update with canonical block, skipping");
                Ok(None)
            }
        }
    }

    fn process_flashblock(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblock: Flashblock,
    ) -> eyre::Result<Option<Arc<PendingBlocks>>> {
        match &prev_pending_blocks {
            Some(pending_blocks) => {
                if self.is_next_flashblock(pending_blocks, &flashblock) {
                    // We have received the next flashblock for the current block
                    // or the first flashblock for the next block
                    let mut flashblocks = pending_blocks.get_flashblocks();
                    flashblocks.push(flashblock);
                    self.build_pending_state(prev_pending_blocks, &flashblocks)
                } else if pending_blocks.latest_block_number() != flashblock.metadata.block_number {
                    // We have received a non-zero flashblock for a new block
                    self.metrics.unexpected_block_order.increment(1);
                    error!(
                        message = "Received non-zero index Flashblock for new block, zeroing Flashblocks until we receive a base Flashblock",
                        curr_block = %pending_blocks.latest_block_number(),
                        new_block = %flashblock.metadata.block_number,
                    );
                    Ok(None)
                } else if pending_blocks.latest_flashblock_index() == flashblock.index {
                    // We have received a duplicate flashblock for the current block
                    self.metrics.unexpected_block_order.increment(1);
                    warn!(
                        message = "Received duplicate Flashblock for current block, ignoring",
                        curr_block = %pending_blocks.latest_block_number(),
                        flashblock_index = %flashblock.index,
                    );
                    Ok(prev_pending_blocks)
                } else {
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
            None => {
                if flashblock.index == 0 {
                    self.build_pending_state(None, &vec![flashblock])
                } else {
                    info!(message = "waiting for first Flashblock");
                    Ok(None)
                }
            }
        }
    }

    fn build_pending_state(
        &self,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        flashblocks: &Vec<Flashblock>,
    ) -> eyre::Result<Option<Arc<PendingBlocks>>> {
        // BTreeMap guarantees ascending order of keys while iterating
        let mut flashblocks_per_block = BTreeMap::<BlockNumber, Vec<&Flashblock>>::new();
        for flashblock in flashblocks {
            flashblocks_per_block
                .entry(flashblock.metadata.block_number)
                .or_default()
                .push(flashblock);
        }

        let earliest_block_number = flashblocks_per_block.keys().min().unwrap();
        let canonical_block = earliest_block_number - 1;
        let mut last_block_header = self.client.header_by_number(canonical_block)?.ok_or(eyre!(
            "Failed to extract header for canonical block number {}. This can be ignored if the node has recently restarted, restored from a snapshot or is still syncing.",
            canonical_block
        ))?;

        let evm_config = OpEvmConfig::optimism(self.client.chain_spec());
        let state_provider =
            self.client.state_by_block_number_or_tag(BlockNumberOrTag::Number(canonical_block))?;
        let state_provider_db = StateProviderDatabase::new(state_provider);
        let mut pending_blocks_builder = PendingBlocksBuilder::new();

        // Cache reads across flashblocks, accumulating caches from previous
        // pending blocks if available
        let cache_db = match &prev_pending_blocks {
            Some(pending_blocks) => CacheDB {
                cache: pending_blocks.get_db_cache(),
                db: state_provider_db,
            },
            None => CacheDB::new(state_provider_db),
        };

        // Track state changes across flashblocks, accumulating bundle state
        // from previous pending blocks if available
        let mut db = match &prev_pending_blocks {
            Some(pending_blocks) => State::builder()
                .with_database(cache_db)
                .with_bundle_update()
                .with_bundle_prestate(pending_blocks.get_bundle_state())
                .build(),
            None => State::builder()
                .with_database(cache_db)
                .with_bundle_update()
                .build(),
        };

        let mut state_overrides =
            prev_pending_blocks.as_ref().map_or_else(StateOverride::default, |pending_blocks| {
                pending_blocks.get_state_overrides().unwrap_or_default()
            });

        for (_block_number, flashblocks) in flashblocks_per_block {
            let base = flashblocks
                .first()
                .ok_or(eyre!("cannot build a pending block from no flashblocks"))?
                .base
                .clone()
                .ok_or(eyre!("first flashblock does not contain a base"))?;

            let latest_flashblock = flashblocks
                .last()
                .cloned()
                .ok_or(eyre!("cannot build a pending block from no flashblocks"))?;

            let transactions: Vec<Bytes> = flashblocks
                .iter()
                .flat_map(|flashblock| flashblock.diff.transactions.clone())
                .collect();

            let withdrawals: Vec<Withdrawal> = flashblocks
                .iter()
                .flat_map(|flashblock| flashblock.diff.withdrawals.clone())
                .collect();

            let receipt_by_hash = flashblocks
                .iter()
                .map(|flashblock| flashblock.metadata.receipts.clone())
                .fold(HashMap::default(), |mut acc, receipts| {
                    acc.extend(receipts);
                    acc
                });

            let updated_balances = flashblocks
                .iter()
                .map(|flashblock| flashblock.metadata.new_account_balances.clone())
                .fold(HashMap::default(), |mut acc, balances| {
                    acc.extend(balances);
                    acc
                });

            pending_blocks_builder.with_flashblocks(
                flashblocks.iter().map(|&x| x.clone()).collect::<Vec<Flashblock>>(),
            );

            let execution_payload: ExecutionPayloadV3 = ExecutionPayloadV3 {
                blob_gas_used: 0,
                excess_blob_gas: 0,
                payload_inner: ExecutionPayloadV2 {
                    withdrawals,
                    payload_inner: ExecutionPayloadV1 {
                        parent_hash: base.parent_hash,
                        fee_recipient: base.fee_recipient,
                        state_root: latest_flashblock.diff.state_root,
                        receipts_root: latest_flashblock.diff.receipts_root,
                        logs_bloom: latest_flashblock.diff.logs_bloom,
                        prev_randao: base.prev_randao,
                        block_number: base.block_number,
                        gas_limit: base.gas_limit,
                        gas_used: latest_flashblock.diff.gas_used,
                        timestamp: base.timestamp,
                        extra_data: base.extra_data.clone(),
                        base_fee_per_gas: base.base_fee_per_gas,
                        block_hash: latest_flashblock.diff.block_hash,
                        transactions,
                    },
                },
            };

            let block: OpBlock = execution_payload.try_into_block()?;
            let mut l1_block_info = reth_optimism_evm::extract_l1_info(&block.body)?;
            let header = block.header.clone().seal_slow();
            pending_blocks_builder.with_header(header.clone());

            let block_env_attributes = OpNextBlockEnvAttributes {
                timestamp: base.timestamp,
                suggested_fee_recipient: base.fee_recipient,
                prev_randao: base.prev_randao,
                gas_limit: base.gas_limit,
                parent_beacon_block_root: Some(base.parent_beacon_block_root),
                extra_data: base.extra_data.clone(),
            };

            let evm_env = evm_config.next_evm_env(&last_block_header, &block_env_attributes)?;
            let mut evm = evm_config.evm_with_env(db, evm_env);

            let mut gas_used = 0;
            let mut next_log_index = 0;

            for (idx, transaction) in block.body.transactions.iter().enumerate() {
                let tx_hash = transaction.tx_hash();

                let sender = match prev_pending_blocks
                    .as_ref()
                    .and_then(|p| p.get_transaction_sender(&tx_hash))
                {
                    Some(sender) => sender,
                    None => transaction.recover_signer()?,
                };

                pending_blocks_builder.with_transaction_sender(tx_hash, sender);
                pending_blocks_builder.increment_nonce(sender);

                let receipt = receipt_by_hash
                    .get(&tx_hash)
                    .cloned()
                    .ok_or(eyre!("missing receipt for {:?}", tx_hash))?;

                let recovered_transaction = Recovered::new_unchecked(transaction.clone(), sender);
                let envelope = recovered_transaction.clone().convert::<OpTxEnvelope>();

                // Build Transaction
                let (deposit_receipt_version, deposit_nonce) = if transaction.is_deposit() {
                    let deposit_receipt = receipt
                        .as_deposit_receipt()
                        .ok_or(eyre!("deposit transaction, non deposit receipt"))?;

                    (deposit_receipt.deposit_receipt_version, deposit_receipt.deposit_nonce)
                } else {
                    (None, None)
                };

                let effective_gas_price = if transaction.is_deposit() {
                    0
                } else {
                    block
                        .base_fee_per_gas
                        .map(|base_fee| {
                            transaction.effective_tip_per_gas(base_fee).unwrap_or_default()
                                + base_fee as u128
                        })
                        .unwrap_or_else(|| transaction.max_fee_per_gas())
                };

                let rpc_txn = Transaction {
                    inner: alloy_rpc_types_eth::Transaction {
                        inner: envelope,
                        block_hash: Some(header.hash()),
                        block_number: Some(base.block_number),
                        transaction_index: Some(idx as u64),
                        effective_gas_price: Some(effective_gas_price),
                    },
                    deposit_nonce,
                    deposit_receipt_version,
                };

                pending_blocks_builder.with_transaction(rpc_txn);

                // Receipt Generation
                let op_receipt = prev_pending_blocks
                    .as_ref()
                    .and_then(|pending_blocks| pending_blocks.get_receipt(tx_hash))
                    .unwrap_or_else(|| {
                        let meta = TransactionMeta {
                            tx_hash,
                            index: idx as u64,
                            block_hash: header.hash(),
                            block_number: block.number,
                            base_fee: block.base_fee_per_gas,
                            excess_blob_gas: block.excess_blob_gas,
                            timestamp: block.timestamp,
                        };

                        let input: ConvertReceiptInput<'_, OpPrimitives> = ConvertReceiptInput {
                            receipt: receipt.clone(),
                            tx: Recovered::new_unchecked(transaction, sender),
                            gas_used: receipt.cumulative_gas_used() - gas_used,
                            next_log_index,
                            meta,
                        };

                        OpReceiptBuilder::new(
                            self.client.chain_spec().as_ref(),
                            input,
                            &mut l1_block_info,
                        )
                        .unwrap()
                        .build()
                    });

                pending_blocks_builder.with_receipt(tx_hash, op_receipt);
                gas_used = receipt.cumulative_gas_used();
                next_log_index += receipt.logs().len();

                let mut should_execute_transaction = true;
                if let Some(state) =
                    prev_pending_blocks.as_ref().and_then(|p| p.get_transaction_state(&tx_hash))
                {
                    pending_blocks_builder.with_transaction_state(tx_hash, state);
                    should_execute_transaction = false;
                }

                if should_execute_transaction {
                    match evm.transact(recovered_transaction) {
                        Ok(ResultAndState { state, .. }) => {
                            for (addr, acc) in &state {
                                let existing_override = state_overrides.entry(*addr).or_default();
                                existing_override.balance = Some(acc.info.balance);
                                existing_override.nonce = Some(acc.info.nonce);
                                existing_override.code =
                                    acc.info.code.clone().map(|code| code.bytes());

                                let existing =
                                    existing_override.state_diff.get_or_insert(Default::default());
                                let changed_slots = acc.storage.iter().map(|(&key, slot)| {
                                    (B256::from(key), B256::from(slot.present_value))
                                });

                                existing.extend(changed_slots);
                            }
                            pending_blocks_builder.with_transaction_state(tx_hash, state.clone());
                            evm.db_mut().commit(state);
                        }
                        Err(e) => {
                            return Err(eyre!(
                                "failed to execute transaction: {:?} tx_hash: {:?} sender: {:?}",
                                e,
                                tx_hash,
                                sender
                            ));
                        }
                    }
                }
            }

            for (address, balance) in updated_balances {
                pending_blocks_builder.with_account_balance(address, balance);
            }

            db = evm.into_db();
            last_block_header = block.header.clone();
        }

        db.merge_transitions(BundleRetention::Reverts);
        pending_blocks_builder.with_bundle_state(db.take_bundle());
        pending_blocks_builder.with_db_cache(db.database.cache);
        pending_blocks_builder.with_state_overrides(state_overrides);
        Ok(Some(Arc::new(pending_blocks_builder.build()?)))
    }

    fn is_next_flashblock(
        &self,
        pending_blocks: &Arc<PendingBlocks>,
        flashblock: &Flashblock,
    ) -> bool {
        let is_next_of_block = flashblock.metadata.block_number
            == pending_blocks.latest_block_number()
            && flashblock.index == pending_blocks.latest_flashblock_index() + 1;
        let is_first_of_next_block = flashblock.metadata.block_number
            == pending_blocks.latest_block_number() + 1
            && flashblock.index == 0;

        is_next_of_block || is_first_of_next_block
    }
}
