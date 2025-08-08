use crate::metrics::Metrics;
use crate::pending::{PendingBlock, PendingBlockBuilder, PendingView, PendingWriter};
use crate::rpc::FlashblocksAPI;
use crate::subscription::{Flashblock, FlashblocksReceiver};
use alloy_consensus::transaction::{Recovered, SignerRecoverable, TransactionMeta};
use alloy_consensus::{Header, TxReceipt};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::map::foldhash::HashMap;
use alloy_primitives::map::B256HashMap;
use alloy_primitives::{Address, BlockNumber, Sealable, TxHash, B256, U256};
use alloy_provider::network::primitives::BlockTransactions;
use alloy_rpc_types::TransactionTrait;
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use alloy_rpc_types_eth::state::{AccountOverride, StateOverride, StateOverridesBuilder};
use alloy_rpc_types_eth::Header as RPCHeader;
use arc_swap::ArcSwapOption;
use eyre::eyre;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_network::Optimism;
use op_alloy_rpc_types::{OpTransactionReceipt, Transaction};
use reth::chainspec::{ChainSpecProvider, EthChainSpec};
use reth::providers::{BlockReaderIdExt, StateProviderFactory};
use reth::revm::context::result::ResultAndState;
use reth::revm::database::StateProviderDatabase;
use reth::revm::{DatabaseCommit, State};
use reth_evm::{ConfigureEvm, Evm};
use reth_optimism_chainspec::OpHardforks;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_primitives::{DepositReceipt, OpBlock, OpPrimitives};
use reth_optimism_rpc::OpReceiptBuilder;
use reth_rpc_convert::transaction::ConvertReceiptInput;
use reth_rpc_convert::RpcTransaction;
use reth_rpc_eth_api::{RpcBlock, RpcReceipt};
use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;
use tokio::sync::broadcast::Sender;
use tracing::{debug, error, info};

// Buffer 4s of Flashblocks
const BUFFER_SIZE: usize = 20;

#[derive(Debug, Clone)]
pub struct FlashblocksState<Client> {
    pending: Arc<ArcSwapOption<PendingBlock>>,
    flashblock_sender: Sender<Flashblock>,
    metrics: Metrics,
    client: Client,
}

impl<Client> FlashblocksState<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + OpHardforks>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + 'static,
{
    pub fn new(client: Client) -> Self {
        Self {
            pending: Arc::new(ArcSwapOption::new(None)),
            flashblock_sender: broadcast::channel(BUFFER_SIZE).0,
            metrics: Metrics::default(),
            client,
        }
    }

    /// Install/replace the current pending snapshot and record what we replaced.
    pub fn set_view(&self, view: PendingBlock) {
        if let Some(prev) = self.pending.swap(Some(std::sync::Arc::new(view))) {
            // ⬇️ you have the previous snapshot here: perfect for metrics/logs
            self.metrics.pending_set.increment(1);
            self.metrics
                .pending_snapshot_height
                .set(prev.block_number() as f64);
            self.metrics
                .pending_snapshot_fb_index
                .set(prev.flashblock_index() as f64);
            // (Optional) tx count, etc., if you want:
            // self.metrics.last_replaced_tx_count.set(prev.get_block(false).transactions.len() as i64);
        }
    }

    /// Clear the pending snapshot unconditionally (and record it if there was one).
    pub fn clear(&self) {
        if let Some(prev) = self.pending.swap(None) {
            self.metrics.pending_clear_catchup.increment(1);
            self.metrics
                .pending_snapshot_height
                .set(prev.block_number() as f64);
            self.metrics
                .pending_snapshot_fb_index
                .set(prev.flashblock_index() as f64);
        }
    }

    /// Clear only when canonical has caught up to (or passed) the pending block number.
    pub fn clear_on_canonical_catchup(&self, canon_number: u64) {
        if let Some(cur) = self.pending.load_full() {
            if cur.block_number() <= canon_number {
                if let Some(prev) = self.pending.swap(None) {
                    self.metrics.pending_clear_catchup.increment(1);
                    self.metrics
                        .pending_snapshot_height
                        .set(prev.block_number() as f64);
                    self.metrics
                        .pending_snapshot_fb_index
                        .set(prev.flashblock_index() as f64);
                }
            }
        }
    }

    fn is_next_flashblock(
        &self,
        pending_block: &Arc<PendingBlock>,
        flashblock: &Flashblock,
    ) -> bool {
        flashblock.metadata.block_number == pending_block.block_number()
            && flashblock.index == pending_block.flashblock_index() + 1
    }

    fn update_block(&self, flashblocks: Vec<Flashblock>) {
        let start_time = Instant::now();
        match self.process_flashblock(flashblocks) {
            Ok(block) => {
                self.set_view(block);
                self.metrics
                    .block_processing_duration
                    .record(start_time.elapsed());
            }
            Err(e) => {
                error!(message = "could not process Flashblock", error = %e);
                self.metrics.block_processing_error.increment(1);
            }
        }
    }

    fn process_flashblock(&self, flashblocks: Vec<Flashblock>) -> eyre::Result<PendingBlock> {
        let mut pending_block_builder = PendingBlockBuilder::new();

        let base = flashblocks
            .first()
            .ok_or(eyre!("cannot build a pendingblock from no flashblocks"))?
            .base
            .clone()
            .ok_or(eyre!("first flashblock does not contain a base"))?;

        let latest_flashblock = flashblocks.last().cloned().unwrap(); // Must have a last Flashblock if we have a first

        let transactions = flashblocks
            .iter()
            .flat_map(|flashblock| flashblock.diff.transactions.clone())
            .collect();

        let withdrawals = flashblocks
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

        pending_block_builder.with_flashblocks(flashblocks);

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
        pending_block_builder.with_header(header.clone());

        let mut gas_used = 0;
        let mut next_log_index = 0;

        let previous_block = header.number - 1;
        let state = self
            .client
            .state_by_block_number_or_tag(BlockNumberOrTag::Number(previous_block))?;
        let state = StateProviderDatabase::new(state);
        let db = State::builder()
            .with_database(state)
            .with_bundle_update()
            .build();

        let block_env_attributes = OpNextBlockEnvAttributes {
            timestamp: base.timestamp,
            suggested_fee_recipient: base.fee_recipient,
            prev_randao: base.prev_randao,
            gas_limit: base.gas_limit,
            parent_beacon_block_root: Some(base.parent_beacon_block_root),
            extra_data: base.extra_data.clone(),
        };
        let previous_header = self.client.header_by_number(previous_block)?.ok_or(eyre!(
            "Failed to extract header for block number {}. Skipping eth_call override setting",
            previous_block
        ))?;

        let evm_config = OpEvmConfig::optimism(self.client.chain_spec());
        let evm_env = evm_config.next_evm_env(&previous_header, &block_env_attributes)?;

        let mut evm = evm_config.evm_with_env(db, evm_env);
        let mut state_cache_builder = StateOverridesBuilder::default();

        for (idx, transaction) in block.body.transactions.iter().enumerate() {
            let sender = match transaction.recover_signer() {
                Ok(signer) => signer,
                Err(err) => return Err(err.into()),
            };

            pending_block_builder.increment_nonce(sender);

            let receipt = receipt_by_hash
                .get(&transaction.tx_hash())
                .cloned()
                .ok_or(eyre!("missing receipt for {:?}", transaction.tx_hash()))?;

            let recovered_transaction = Recovered::new_unchecked(transaction.clone(), sender);
            let envelope = recovered_transaction.clone().convert::<OpTxEnvelope>();

            // Build Transaction
            let (deposit_receipt_version, deposit_nonce) = if transaction.is_deposit() {
                let deposit_receipt = receipt
                    .as_deposit_receipt()
                    .ok_or(eyre!("deposit transaction, non deposit receipt"))?;

                (
                    deposit_receipt.deposit_receipt_version,
                    deposit_receipt.deposit_nonce,
                )
            } else {
                (None, None)
            };

            let effective_gas_price = if transaction.is_deposit() {
                0
            } else {
                block
                    .base_fee_per_gas
                    .map(|base_fee| {
                        transaction
                            .effective_tip_per_gas(base_fee)
                            .unwrap_or_default()
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

            pending_block_builder.with_transaction(rpc_txn);
            // End Transaction

            // Receipt Generation
            let meta = TransactionMeta {
                tx_hash: transaction.tx_hash(),
                index: idx as u64,
                block_hash: header.hash(),
                block_number: block.number,
                base_fee: block.base_fee_per_gas,
                excess_blob_gas: block.excess_blob_gas,
                timestamp: block.timestamp,
            };

            let input: ConvertReceiptInput<'_, OpPrimitives> = ConvertReceiptInput {
                receipt: Cow::Borrowed(&receipt),
                tx: Recovered::new_unchecked(transaction, sender),
                gas_used: receipt.cumulative_gas_used() - gas_used,
                next_log_index,
                meta,
            };

            let op_receipt = OpReceiptBuilder::new(
                self.client.chain_spec().as_ref(),
                input,
                &mut l1_block_info,
            )?
            .build();

            pending_block_builder.with_receipt(transaction.tx_hash(), op_receipt);

            gas_used = receipt.cumulative_gas_used();
            next_log_index += receipt.logs().len();

            // EVM Transaction
            let ResultAndState { state, .. } = evm.transact(recovered_transaction)?;
            for (addr, acc) in &state {
                let state_diff = B256HashMap::<B256>::from_iter(
                    acc.storage
                        .iter()
                        .map(|(&key, slot)| (key.into(), slot.present_value.into())),
                );
                let acc_override = AccountOverride {
                    balance: Some(acc.info.balance),
                    nonce: Some(acc.info.nonce),
                    code: acc.info.code.clone().map(|code| code.bytes()),
                    state: None,
                    state_diff: Some(state_diff),
                    move_precompile_to: None,
                };
                state_cache_builder = state_cache_builder.append(*addr, acc_override);
            }
            evm.db_mut().commit(state);
            // End EVM Transaction
        }

        pending_block_builder.with_state_overrides(state_cache_builder.build());

        for (address, balance) in updated_balances {
            pending_block_builder.with_account_balance(address, balance);
        }

        pending_block_builder.build()
    }
}

impl<Client> FlashblocksReceiver for FlashblocksState<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + OpHardforks>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + 'static,
{
    fn on_flashblock_received(&self, flashblock: Flashblock) {
        match self.pending.load_full() {
            Some(pending_block) => {
                if flashblock.index == 0 {
                    self.metrics
                        .flashblocks_in_block
                        .record((pending_block.flashblock_index() + 1) as f64);

                    self.update_block(vec![flashblock.clone()]);
                } else if self.is_next_flashblock(&pending_block, &flashblock) {
                    let mut flashblocks = pending_block.get_flashblocks();
                    flashblocks.push(flashblock.clone());

                    self.update_block(flashblocks);
                } else if pending_block.block_number() != flashblock.metadata.block_number {
                    self.metrics.unexpected_block_order.increment(1);
                    self.pending.swap(None);

                    error!(
                        message = "Received Flashblock for new block, zeroing Flashblocks until we receive a base Flashblock",
                        curr_block = %pending_block.block_number(),
                        new_block = %flashblock.metadata.block_number,
                    );
                } else {
                    self.metrics.unexpected_block_order.increment(1);

                    info!(
                        message = "None sequential Flashblocks, keeping cache",
                        curr_block = %pending_block.block_number(),
                        new_block = %flashblock.metadata.block_number,
                    );
                }
            }
            None => {
                if flashblock.index == 0 {
                    self.update_block(vec![flashblock.clone()]);
                } else {
                    debug!(message = "waiting for first Flashblock")
                }
            }
        }

        _ = self.flashblock_sender.send(flashblock);
    }
}

impl<Client> PendingView for FlashblocksState<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + OpHardforks>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn block_number(&self) -> BlockNumber {
        self.pending
            .load_full()
            .map(|pb| pb.block_number())
            .unwrap_or_default()
    }

    fn flashblock_index(&self) -> u64 {
        self.pending
            .load_full()
            .map(|pb| pb.flashblock_index())
            .unwrap_or_default()
    }

    fn get_block(&self, full: bool) -> RpcBlock<Optimism> {
        self.pending
            .load_full()
            .map(|pb| pb.get_block(full))
            .unwrap_or_else(|| {
                // Return empty block if no pending block
                RpcBlock::<Optimism> {
                    header: RPCHeader::default(),
                    transactions: BlockTransactions::Hashes(vec![]),
                    uncles: Vec::new(),
                    withdrawals: None,
                }
            })
    }

    fn get_receipt(&self, tx_hash: TxHash) -> Option<OpTransactionReceipt> {
        self.pending
            .load_full()
            .and_then(|pb| pb.get_receipt(tx_hash))
    }

    fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<Transaction> {
        self.pending
            .load_full()
            .and_then(|pb| pb.get_transaction_by_hash(tx_hash))
    }

    fn get_transaction_count(&self, address: Address) -> U256 {
        self.pending
            .load_full()
            .map(|pb| pb.get_transaction_count(address))
            .unwrap_or_default()
    }

    fn get_balance(&self, address: Address) -> Option<U256> {
        self.pending
            .load_full()
            .and_then(|pb| pb.get_balance(address))
    }

    fn get_state_overrides(&self) -> Option<StateOverride> {
        self.pending
            .load_full()
            .and_then(|pb| pb.get_state_overrides())
    }
}

impl<Client> PendingWriter for FlashblocksState<Client>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec: EthChainSpec<Header = Header> + OpHardforks>
        + BlockReaderIdExt<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn on_flashblock_received(&self, fb: Flashblock) {
        // Delegate to the existing FlashblocksReceiver implementation
        FlashblocksReceiver::on_flashblock_received(self, fb);
    }

    fn set_view(&self, pb: PendingBlock) {
        if let Some(prev) = self.pending.swap(Some(Arc::new(pb))) {
            self.metrics.pending_set.increment(1);
            self.metrics
                .pending_snapshot_height
                .set(prev.block_number() as f64);
            self.metrics
                .pending_snapshot_fb_index
                .set(prev.flashblock_index() as f64);
        }
    }

    fn clear(&self) {
        if let Some(prev) = self.pending.swap(None) {
            self.metrics.pending_clear_catchup.increment(1);
            self.metrics
                .pending_snapshot_height
                .set(prev.block_number() as f64);
            self.metrics
                .pending_snapshot_fb_index
                .set(prev.flashblock_index() as f64);
        }
    }

    fn clear_on_canonical_catchup(&self, canon: u64) {
        if let Some(cur) = self.pending.load_full() {
            if cur.block_number() <= canon {
                if let Some(prev) = self.pending.swap(None) {
                    self.metrics.pending_clear_catchup.increment(1);
                    self.metrics
                        .pending_snapshot_height
                        .set(prev.block_number() as f64);
                    self.metrics
                        .pending_snapshot_fb_index
                        .set(prev.flashblock_index() as f64);
                }
            }
        }
    }
}

impl<Client> FlashblocksAPI for FlashblocksState<Client> {
    fn get_block(&self, full: bool) -> Option<RpcBlock<Optimism>> {
        self.pending.load_full().map(|pb| pb.get_block(full))
    }

    fn get_transaction_receipt(&self, tx_hash: TxHash) -> Option<RpcReceipt<Optimism>> {
        self.pending
            .load_full()
            .and_then(|pb| pb.get_receipt(tx_hash))
    }

    fn get_transaction_count(&self, address: Address) -> U256 {
        self.pending
            .load_full()
            .map(|pb| pb.get_transaction_count(address))
            .unwrap_or_else(|| U256::from(0))
    }

    fn get_transaction_by_hash(&self, tx_hash: TxHash) -> Option<RpcTransaction<Optimism>> {
        self.pending
            .load_full()
            .and_then(|pb| pb.get_transaction_by_hash(tx_hash))
    }

    fn get_balance(&self, address: Address) -> Option<U256> {
        self.pending
            .load_full()
            .and_then(|pb| pb.get_balance(address))
    }

    fn subscribe_to_flashblocks(&self) -> broadcast::Receiver<Flashblock> {
        self.flashblock_sender.subscribe()
    }

    fn get_state_overrides(&self) -> Option<StateOverride> {
        self.pending
            .load_full()
            .and_then(|pb| pb.get_state_overrides())
    }
}
