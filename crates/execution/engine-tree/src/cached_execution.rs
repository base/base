//! Cached execution provider and executor.

use std::{collections::HashMap, fmt::Debug, sync::Arc};

use base_common_consensus::{BaseReceipt, BaseTxEnvelope, OpTxType};
use base_common_evm::{BaseBlockExecutor, BaseHaltReason, BaseTransaction, BaseTxResult};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseRethReceiptBuilder;
use base_flashblocks::{FlashblocksAPI, FlashblocksState};
use reth_errors::BlockExecutionError;
use reth_evm::{
    Evm, RecoveredTx,
    block::{BlockExecutor, ExecutableTx, InternalBlockExecutionError, TxResult},
};
use reth_primitives_traits::Recovered;
use reth_revm::State;
use revm::{Database, context::TxEnv};
use revm_primitives::B256;
use tracing::{instrument, trace, warn};

/// Provider that fetches cached execution results for transactions.
#[derive(Debug, Clone)]
pub struct FlashblocksCachedExecutionProvider {
    flashblocks_state: Option<Arc<FlashblocksState>>,
}

impl FlashblocksCachedExecutionProvider {
    /// Creates a new [`FlashblocksCachedExecutionProvider`].
    pub const fn new(flashblocks_state: Option<Arc<FlashblocksState>>) -> Self {
        Self { flashblocks_state }
    }
}

impl CachedExecutionProvider<BaseTxResult<BaseHaltReason, OpTxType>>
    for FlashblocksCachedExecutionProvider
{
    #[instrument(level = "debug", skip_all, fields(tx_hash = ?tx_hash))]
    fn get_cached_execution_for_tx(
        &self,
        parent_block_hash: &B256,
        prev_cached_hash: Option<&B256>,
        tx_hash: &B256,
    ) -> Option<BaseTxResult<BaseHaltReason, OpTxType>> {
        let flashblocks_state = self.flashblocks_state.as_ref()?;
        let pending_blocks = flashblocks_state.get_pending_blocks().clone()?;

        // The cached results were computed on top of a specific parent block.
        // During a reorg or sequencer failover, two different parent hashes can
        // share the same block number, so verifying block-number equivalence
        // (e.g. via a separate `BlockNumReader` lookup) is insufficient — we
        // must compare the parent hash itself.
        if pending_blocks.parent_hash() != *parent_block_hash {
            warn!(
                expected_parent = ?pending_blocks.parent_hash(),
                received_parent = ?parent_block_hash,
                "Not using cached results - parent hash mismatch (possible reorg or sequencer failover)",
            );
            return None;
        }

        let this_block_number = pending_blocks.earliest_block_number();

        // The cached `ResultAndState` is only valid when applied atop the exact
        // prefix it was computed under, so require `tx_hash` to occupy the
        // immediate successor position to `prev_cached_hash` in the cached order.
        let Some(this_pos) = pending_blocks.transaction_position(this_block_number, tx_hash) else {
            warn!(
                tx_hash = ?tx_hash,
                "Not using cached results - transaction not cached for this block",
            );
            return None;
        };
        let positions_align = prev_cached_hash.map_or(this_pos == 0, |prev| {
            pending_blocks
                .transaction_position(this_block_number, prev)
                .is_some_and(|prev_pos| prev_pos + 1 == this_pos)
        });
        if !positions_align {
            warn!(
                tx_hash = ?tx_hash,
                prev_cached_hash = ?prev_cached_hash,
                this_pos = this_pos,
                "Not using cached results - transaction is not the expected successor in cached order",
            );
            return None;
        }

        trace!(tx_hash = ?tx_hash, "cache hit for transaction");
        pending_blocks.get_tx_result(tx_hash)
    }
}

/// Trait for providers that fetch cached execution results for transactions.
///
/// Callers must invoke in payload order and stop on the first `None`, so that any
/// returned cached result is applied atop the same prefix it was computed under.
pub trait CachedExecutionProvider<TxResult> {
    /// Gets the cached execution result for a transaction.
    fn get_cached_execution_for_tx(
        &self,
        parent_block_hash: &B256,
        prev_cached_hash: Option<&B256>,
        tx_hash: &B256,
    ) -> Option<TxResult>;
}

/// Default implementation of [`CachedExecutionProvider`] that does not provide any cached execution.
#[derive(Debug, Clone, Default)]
pub struct NoopCachedExecutionProvider;

impl<TxResult> CachedExecutionProvider<TxResult> for NoopCachedExecutionProvider {
    fn get_cached_execution_for_tx(
        &self,
        _parent_block_hash: &B256,
        _prev_cached_hash: Option<&B256>,
        _tx_hash: &B256,
    ) -> Option<TxResult> {
        None
    }
}

/// Executor that fetches cached execution results for transactions.
#[derive(Debug)]
pub struct CachedExecutor<E, C> {
    executor: BaseBlockExecutor<E, BaseRethReceiptBuilder, Arc<BaseChainSpec>>,
    cached_execution_provider: C,
    txs: Vec<B256>,
    position_by_hash: HashMap<B256, usize>,
    parent_block_hash: B256,
    all_txs_cached: bool,
}

impl<E, C> CachedExecutor<E, C> {
    /// Creates a new [`CachedExecutor`].
    pub fn new(
        executor: BaseBlockExecutor<E, BaseRethReceiptBuilder, Arc<BaseChainSpec>>,
        cached_execution_provider: C,
        txs: Vec<B256>,
        parent_block_hash: B256,
    ) -> Self {
        let position_by_hash =
            txs.iter().enumerate().map(|(i, tx)| (*tx, i)).collect::<HashMap<_, _>>();
        Self {
            executor,
            cached_execution_provider,
            txs,
            position_by_hash,
            parent_block_hash,
            all_txs_cached: true,
        }
    }
}

impl<'a, DB, E, C> BlockExecutor for CachedExecutor<E, C>
where
    DB: Database + alloy_evm::Database + 'a,
    E: Evm<DB = &'a mut State<DB>, Tx = BaseTransaction<TxEnv>>,
    C: CachedExecutionProvider<BaseTxResult<E::HaltReason, OpTxType>>,
{
    type Transaction = BaseTxEnvelope;
    type Receipt = BaseReceipt;
    type Evm = E;
    type Result = BaseTxResult<E::HaltReason, OpTxType>;

    fn receipts(&self) -> &[Self::Receipt] {
        self.executor.receipts()
    }

    #[instrument(level = "debug", skip_all)]
    fn execute_transaction_without_commit(
        &mut self,
        executing_tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        if !self.all_txs_cached {
            return self.executor.execute_transaction_without_commit(executing_tx);
        }

        let executing_tx_recovered = executing_tx.into_parts().1;
        let tx_hash = executing_tx_recovered.tx().tx_hash();

        // find tx just before this one
        let tx_position = self.position_by_hash.get(&tx_hash);

        // not found, we need to execute the transaction
        let Some(tx_position) = tx_position else {
            self.all_txs_cached = false;
            return self.executor.execute_transaction_without_commit(Recovered::new_unchecked(
                executing_tx_recovered.tx(),
                *executing_tx_recovered.signer(),
            ));
        };

        let prev_tx_hash = tx_position.checked_sub(1).and_then(|pos| self.txs.get(pos));

        let cached_execution = self.cached_execution_provider.get_cached_execution_for_tx(
            &self.parent_block_hash,
            prev_tx_hash,
            &tx_hash,
        );
        if let Some(cached_execution) = cached_execution {
            // load accounts into cache
            for address in cached_execution.result().state.keys() {
                // ignore the result since we don't care if the account exists or not
                self.executor.evm_mut().db_mut().load_cache_account(*address).map_err(|err| {
                    BlockExecutionError::Internal(InternalBlockExecutionError::Other(Box::new(err)))
                })?;
            }
            return Ok(cached_execution);
        }
        self.all_txs_cached = false;
        self.executor.execute_transaction_without_commit(Recovered::new_unchecked(
            executing_tx_recovered.tx(),
            *executing_tx_recovered.signer(),
        ))
    }

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.executor.apply_pre_execution_changes()
    }

    fn commit_transaction(&mut self, output: Self::Result) -> Result<u64, BlockExecutionError> {
        self.executor.commit_transaction(output)
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, reth_provider::BlockExecutionResult<Self::Receipt>), BlockExecutionError>
    {
        self.executor.finish()
    }

    fn set_state_hook(&mut self, hook: Option<Box<dyn reth_evm::OnStateHook>>) {
        self.executor.set_state_hook(hook)
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.executor.evm_mut()
    }

    fn evm(&self) -> &Self::Evm {
        self.executor.evm()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{
        Header, Receipt, ReceiptWithBloom, Sealed, Signed, TxLegacy, transaction::Recovered,
    };
    use alloy_primitives::{Address, B256, BlockNumber, Bloom, Bytes, Signature, TxKind, U256};
    use alloy_rpc_types_engine::PayloadId;
    use alloy_rpc_types_eth::TransactionReceipt;
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
    };
    use base_common_rpc_types::{BaseTransactionReceipt, L1BlockInfo, Transaction};
    use base_flashblocks::{FlashblocksState, PendingBlocks, PendingBlocksBuilder};
    use revm::context::result::ExecutionResult;

    use super::{
        BaseHaltReason, BaseReceipt, BaseTxEnvelope, CachedExecutionProvider,
        FlashblocksCachedExecutionProvider,
    };

    fn provider_with_cache(
        parent_hash: B256,
        parent_number: BlockNumber,
        cached_hashes: &[B256],
    ) -> FlashblocksCachedExecutionProvider {
        let state = Arc::new(FlashblocksState::default());
        state.set_pending_blocks_for_testing(Some(build_pending_blocks(
            parent_hash,
            parent_number + 1,
            cached_hashes,
        )));
        FlashblocksCachedExecutionProvider::new(Some(state))
    }

    fn h(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn build_pending_blocks(
        parent_hash: B256,
        block_number: BlockNumber,
        tx_hashes: &[B256],
    ) -> PendingBlocks {
        let mut builder = PendingBlocksBuilder::new();
        builder.with_flashblocks([stub_flashblock(parent_hash, block_number)]);
        builder.with_header(Sealed::new_unchecked(
            Header { number: block_number, parent_hash, ..Default::default() },
            B256::ZERO,
        ));
        for &hash in tx_hashes {
            builder.with_transaction(stub_transaction(hash, block_number));
            builder.with_receipt(hash, stub_receipt(hash, block_number));
            builder.with_transaction_state(hash, Default::default());
            builder.with_transaction_sender(hash, Address::ZERO);
            builder.with_transaction_result(hash, stub_execution_result());
        }
        builder.build().expect("test pending blocks should build")
    }

    const fn stub_execution_result() -> ExecutionResult<BaseHaltReason> {
        ExecutionResult::Success {
            reason: revm::context::result::SuccessReason::Stop,
            gas_used: 21_000,
            gas_refunded: 0,
            logs: Vec::new(),
            output: revm::context::result::Output::Call(Bytes::new()),
        }
    }

    fn stub_flashblock(parent_hash: B256, block_number: BlockNumber) -> Flashblock {
        Flashblock {
            payload_id: PayloadId::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number,
                gas_limit: 30_000_000,
                timestamp: 1_700_000_000,
                extra_data: Bytes::default(),
                base_fee_per_gas: U256::from(1_000_000_000u64),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 0,
                block_hash: B256::ZERO,
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: Metadata { block_number },
        }
    }

    fn stub_transaction(hash: B256, block_number: BlockNumber) -> Transaction {
        let legacy = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 1_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let envelope = BaseTxEnvelope::Legacy(Signed::new_unchecked(
            legacy,
            Signature::test_signature(),
            hash,
        ));
        Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: Recovered::new_unchecked(envelope, Address::ZERO),
                block_hash: Some(B256::ZERO),
                block_number: Some(block_number),
                transaction_index: Some(0),
                effective_gas_price: Some(1_000_000_000),
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
        }
    }

    fn stub_receipt(tx_hash: B256, block_number: BlockNumber) -> BaseTransactionReceipt {
        BaseTransactionReceipt {
            inner: TransactionReceipt {
                inner: ReceiptWithBloom {
                    receipt: BaseReceipt::Legacy(Receipt {
                        status: alloy_consensus::Eip658Value::Eip658(true),
                        cumulative_gas_used: 21_000,
                        logs: vec![],
                    }),
                    logs_bloom: Bloom::default(),
                },
                transaction_hash: tx_hash,
                transaction_index: Some(0),
                block_hash: Some(B256::ZERO),
                block_number: Some(block_number),
                gas_used: 21_000,
                effective_gas_price: 1_000_000_000,
                blob_gas_used: None,
                blob_gas_price: None,
                from: Address::ZERO,
                to: None,
                contract_address: None,
            },
            l1_block_info: L1BlockInfo::default(),
        }
    }

    #[test]
    fn honest_path_hits_at_every_position() {
        let parent = h(0xff);
        let (a, b, c) = (h(0x01), h(0x02), h(0x03));
        let provider = provider_with_cache(parent, 1, &[a, b, c]);

        assert!(provider.get_cached_execution_for_tx(&parent, None, &a).is_some());
        assert!(provider.get_cached_execution_for_tx(&parent, Some(&a), &b).is_some());
        assert!(provider.get_cached_execution_for_tx(&parent, Some(&b), &c).is_some());
    }

    /// Models the original report's payload `[deposit, nonce1]` against cache
    /// `[deposit, nonce0, nonce1]` as `[a, c]` against `[a, b, c]`.
    #[test]
    fn skip_middle_returns_none_for_unrelated_successor() {
        let parent = h(0xff);
        let (a, b, c) = (h(0x01), h(0x02), h(0x03));
        let provider = provider_with_cache(parent, 1, &[a, b, c]);

        assert!(provider.get_cached_execution_for_tx(&parent, Some(&a), &c).is_none());
    }

    #[test]
    fn out_of_order_successor_returns_none() {
        let parent = h(0xff);
        let (a, b, c) = (h(0x01), h(0x02), h(0x03));
        let provider = provider_with_cache(parent, 1, &[a, b, c]);

        assert!(provider.get_cached_execution_for_tx(&parent, Some(&b), &a).is_none());
    }

    #[test]
    fn first_tx_must_match_first_cached() {
        let parent = h(0xff);
        let (a, b) = (h(0x01), h(0x02));
        let provider = provider_with_cache(parent, 1, &[a, b]);

        assert!(provider.get_cached_execution_for_tx(&parent, None, &a).is_some());
        assert!(provider.get_cached_execution_for_tx(&parent, None, &b).is_none());
    }

    #[test]
    fn prev_not_in_cache_returns_none() {
        let parent = h(0xff);
        let (a, b, z) = (h(0x01), h(0x02), h(0x09));
        let provider = provider_with_cache(parent, 1, &[a, b]);

        assert!(provider.get_cached_execution_for_tx(&parent, Some(&z), &b).is_none());
    }

    #[test]
    fn prev_is_last_cached_returns_none() {
        let parent = h(0xff);
        let (a, b, c) = (h(0x01), h(0x02), h(0x03));
        let provider = provider_with_cache(parent, 1, &[a, b]);

        assert!(provider.get_cached_execution_for_tx(&parent, Some(&b), &c).is_none());
    }

    #[test]
    fn missing_flashblocks_state_returns_none() {
        let parent = h(0xff);
        let provider = FlashblocksCachedExecutionProvider::new(None);
        assert!(provider.get_cached_execution_for_tx(&parent, None, &h(0x01)).is_none());
    }

    /// Models a reorg or sequencer failover: the cache was built on top of `h1`,
    /// but the engine asks for a payload at the same height built on top of a
    /// different parent `h2`. The cached results were computed against `h1`'s
    /// state and would corrupt the post-state if reused, so the provider must
    /// refuse the cache regardless of block-number agreement.
    #[test]
    fn parent_hash_mismatch_returns_none_after_reorg() {
        let h1 = h(0x11);
        let h2 = h(0x22);
        let a = h(0x01);
        let provider = provider_with_cache(h1, 1, &[a]);

        assert!(provider.get_cached_execution_for_tx(&h1, None, &a).is_some());
        assert!(provider.get_cached_execution_for_tx(&h2, None, &a).is_none());
    }
}
