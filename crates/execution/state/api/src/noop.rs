//! Various noop implementations for traits.

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Debug,
    ops::{RangeBounds, RangeInclusive},
};

use alloy_eips::{BlockHashOrNumber, BlockId, BlockNumberOrTag};
use alloy_primitives::{
    Address, B256, BlockHash, BlockNumber, Bytes, StorageKey, StorageValue, TxHash, TxNumber,
};
use base_common_chain_config::{BaseChainSpec, ChainSpecProvider};
use base_common_types_chain::{
    BaseBlock, BaseReceipt, BaseTxEnvelope, ChainInfo, transaction::TransactionMeta,
};
#[cfg(feature = "db-api")]
use base_execution_state_database::TxMock;
use base_execution_state_memory::{StoredAccount as Account, StoredBytecode as Bytecode};
use base_execution_state_types::ExecutionOutcome;
#[cfg(feature = "db-api")]
use base_execution_state_types::PruneModes;
use base_execution_state_types::{AccountBeforeTx, StoredBlockBodyIndices};
use base_execution_state_types::{
    AccountProof, ExecutionWitnessMode, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput, updates::TrieUpdates,
};
use base_execution_state_types::{ProviderError, ProviderResult};
use base_execution_state_types::{PruneCheckpoint, PruneSegment};
use base_execution_state_types::{StageCheckpoint, StageId};
use reth_primitives_traits::{RecoveredBlock, SealedHeader};

use crate::{
    AccountReader, BalProvider, BalStoreHandle, BlockBodyIndicesProvider, BlockHashReader,
    BlockIdReader, BlockNumReader, BlockReader, BlockReaderIdExt, BlockSource, BytecodeReader,
    ChangeSetReader, HashedPostStateProvider, HeaderProvider, PruneCheckpointReader,
    ReceiptProvider, ReceiptProviderIdExt, StageCheckpointReader, StateProofProvider,
    StateProviderBox, StateProviderFactory, StateRangeProviderFactory, StateRangeView, StateReader,
    StateRootProvider, StorageRootProvider, TransactionVariant, TransactionsProvider,
    TryIntoHistoricalStateProvider,
};
#[cfg(feature = "db-api")]
use crate::{
    DBProvider, DatabaseProviderFactory, DatabaseProviderROFactory, DbTxProvider,
    StorageChangeSetReader, StorageSettingsCache,
};

/// Supports various api interfaces for testing purposes.
#[derive(Debug)]
#[non_exhaustive]
pub struct NoopProvider {
    chain_spec: Arc<BaseChainSpec>,
    bal_store: BalStoreHandle,
    #[cfg(feature = "db-api")]
    tx: TxMock,
    #[cfg(feature = "db-api")]
    prune_modes: PruneModes,
}

impl NoopProvider {
    /// Create a new instance for specific primitive types.
    pub fn new(chain_spec: Arc<BaseChainSpec>) -> Self {
        Self {
            chain_spec,
            bal_store: BalStoreHandle::default(),
            #[cfg(feature = "db-api")]
            tx: TxMock::default(),
            #[cfg(feature = "db-api")]
            prune_modes: PruneModes::default(),
        }
    }
}

impl NoopProvider {
    /// Create a new instance of the `NoopBlockReader`.
    pub fn eth(chain_spec: Arc<BaseChainSpec>) -> Self {
        Self {
            chain_spec,
            bal_store: BalStoreHandle::default(),
            #[cfg(feature = "db-api")]
            tx: TxMock::default(),
            #[cfg(feature = "db-api")]
            prune_modes: PruneModes::default(),
        }
    }
}

impl NoopProvider {
    /// Create a new instance of the [`NoopProvider`] with the mainnet chain spec.
    pub fn mainnet() -> Self {
        Self::eth(Arc::new(BaseChainSpec::mainnet()))
    }
}

impl Default for NoopProvider {
    fn default() -> Self {
        Self::mainnet()
    }
}

impl Clone for NoopProvider {
    fn clone(&self) -> Self {
        Self {
            chain_spec: Arc::clone(&self.chain_spec),
            bal_store: self.bal_store.clone(),
            #[cfg(feature = "db-api")]
            tx: self.tx.clone(),
            #[cfg(feature = "db-api")]
            prune_modes: self.prune_modes.clone(),
        }
    }
}

impl BalProvider for NoopProvider {
    fn bal_store(&self) -> &BalStoreHandle {
        &self.bal_store
    }
}

impl StateRangeProviderFactory for NoopProvider {
    fn state_range_provider(&self, _state_root: B256) -> ProviderResult<Option<StateRangeView>> {
        Ok(None)
    }
}

/// Noop implementation for testing purposes
impl BlockHashReader for NoopProvider {
    fn block_hash(&self, _number: u64) -> ProviderResult<Option<B256>> {
        Ok(None)
    }

    fn canonical_hashes_range(
        &self,
        _start: BlockNumber,
        _end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        Ok(Vec::new())
    }
}

impl BlockNumReader for NoopProvider {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        Ok(ChainInfo::default())
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        Ok(0)
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        Ok(0)
    }

    fn block_number(&self, _hash: B256) -> ProviderResult<Option<BlockNumber>> {
        Ok(None)
    }
}

impl ChainSpecProvider for NoopProvider {
    fn chain_spec(&self) -> Arc<BaseChainSpec> {
        self.chain_spec.clone()
    }
}

impl BlockIdReader for NoopProvider {
    fn pending_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        Ok(None)
    }

    fn safe_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        Ok(None)
    }

    fn finalized_block_num_hash(&self) -> ProviderResult<Option<alloy_eips::BlockNumHash>> {
        Ok(None)
    }
}

impl BlockReaderIdExt for NoopProvider {
    fn block_by_id(&self, _id: BlockId) -> ProviderResult<Option<BaseBlock>> {
        Ok(None)
    }

    fn sealed_header_by_id(&self, _id: BlockId) -> ProviderResult<Option<SealedHeader>> {
        Ok(None)
    }

    fn header_by_id(
        &self,
        _id: BlockId,
    ) -> ProviderResult<Option<base_common_types_chain::Header>> {
        Ok(None)
    }
}

impl BlockReader for NoopProvider {
    type Block = BaseBlock;

    fn find_block_by_hash(
        &self,
        _hash: B256,
        _source: BlockSource,
    ) -> ProviderResult<Option<Self::Block>> {
        Ok(None)
    }

    fn block(&self, _id: BlockHashOrNumber) -> ProviderResult<Option<Self::Block>> {
        Ok(None)
    }

    fn pending_block(&self) -> ProviderResult<Option<RecoveredBlock>> {
        Ok(None)
    }

    fn pending_block_and_receipts(
        &self,
    ) -> ProviderResult<Option<(RecoveredBlock, Vec<BaseReceipt>)>> {
        Ok(None)
    }

    fn recovered_block(
        &self,
        _id: BlockHashOrNumber,
        _transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock>> {
        Ok(None)
    }

    fn sealed_block_with_senders(
        &self,
        _id: BlockHashOrNumber,
        _transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock>> {
        Ok(None)
    }

    fn block_range(&self, _range: RangeInclusive<BlockNumber>) -> ProviderResult<Vec<Self::Block>> {
        Ok(Vec::new())
    }

    fn block_with_senders_range(
        &self,
        _range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock>> {
        Ok(Vec::new())
    }

    fn recovered_block_range(
        &self,
        _range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<RecoveredBlock>> {
        Ok(Vec::new())
    }

    fn block_by_transaction_id(&self, _id: TxNumber) -> ProviderResult<Option<BlockNumber>> {
        Ok(None)
    }
}

impl TransactionsProvider for NoopProvider {
    type Transaction = BaseTxEnvelope;

    fn transaction_id(&self, _tx_hash: TxHash) -> ProviderResult<Option<TxNumber>> {
        Ok(None)
    }

    fn transaction_by_id(&self, _id: TxNumber) -> ProviderResult<Option<Self::Transaction>> {
        Ok(None)
    }

    fn transaction_by_id_unhashed(
        &self,
        _id: TxNumber,
    ) -> ProviderResult<Option<Self::Transaction>> {
        Ok(None)
    }

    fn transaction_by_hash(&self, _hash: TxHash) -> ProviderResult<Option<Self::Transaction>> {
        Ok(None)
    }

    fn transaction_by_hash_with_meta(
        &self,
        _hash: TxHash,
    ) -> ProviderResult<Option<(Self::Transaction, TransactionMeta)>> {
        Ok(None)
    }

    fn transactions_by_block(
        &self,
        _block_id: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Transaction>>> {
        Ok(None)
    }

    fn transactions_by_block_range(
        &self,
        _range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Transaction>>> {
        Ok(Vec::default())
    }

    fn transactions_by_tx_range(
        &self,
        _range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Transaction>> {
        Ok(Vec::default())
    }

    fn senders_by_tx_range(
        &self,
        _range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Address>> {
        Ok(Vec::default())
    }

    fn transaction_sender(&self, _id: TxNumber) -> ProviderResult<Option<Address>> {
        Ok(None)
    }
}

impl ReceiptProvider for NoopProvider {
    fn receipt(&self, _id: TxNumber) -> ProviderResult<Option<BaseReceipt>> {
        Ok(None)
    }

    fn receipt_by_hash(&self, _hash: TxHash) -> ProviderResult<Option<BaseReceipt>> {
        Ok(None)
    }

    fn receipts_by_block(
        &self,
        _block: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<BaseReceipt>>> {
        Ok(None)
    }

    fn receipts_by_tx_range(
        &self,
        _range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<BaseReceipt>> {
        Ok(Vec::new())
    }

    fn receipts_by_block_range(
        &self,
        _block_range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<BaseReceipt>>> {
        Ok(Vec::new())
    }
}

impl ReceiptProviderIdExt for NoopProvider {}

impl HeaderProvider for NoopProvider {
    fn header(
        &self,
        _block_hash: BlockHash,
    ) -> ProviderResult<Option<base_common_types_chain::Header>> {
        Ok(None)
    }

    fn header_by_number(
        &self,
        _num: u64,
    ) -> ProviderResult<Option<base_common_types_chain::Header>> {
        Ok(None)
    }

    fn headers_range(
        &self,
        _range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<base_common_types_chain::Header>> {
        Ok(Vec::new())
    }

    fn sealed_header(&self, _number: BlockNumber) -> ProviderResult<Option<SealedHeader>> {
        Ok(None)
    }

    fn sealed_headers_while(
        &self,
        _range: impl RangeBounds<BlockNumber>,
        _predicate: impl FnMut(&SealedHeader) -> bool,
    ) -> ProviderResult<Vec<SealedHeader>> {
        Ok(Vec::new())
    }
}

impl AccountReader for NoopProvider {
    fn basic_account(&self, _address: &Address) -> ProviderResult<Option<Account>> {
        Ok(None)
    }
}

impl ChangeSetReader for NoopProvider {
    fn account_block_changeset(
        &self,
        _block_number: BlockNumber,
    ) -> ProviderResult<Vec<AccountBeforeTx>> {
        Ok(Vec::default())
    }

    fn get_account_before_block(
        &self,
        _block_number: BlockNumber,
        _address: Address,
    ) -> ProviderResult<Option<AccountBeforeTx>> {
        Ok(None)
    }

    fn account_changesets_range(
        &self,
        _range: impl core::ops::RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<(BlockNumber, AccountBeforeTx)>> {
        Ok(Vec::default())
    }
}

#[cfg(feature = "db-api")]
impl StorageChangeSetReader for NoopProvider {
    fn storage_changeset(
        &self,
        _block_number: BlockNumber,
    ) -> ProviderResult<
        Vec<(
            base_execution_state_database::models::BlockNumberAddress,
            base_execution_state_types::StorageEntry,
        )>,
    > {
        Ok(Vec::default())
    }

    fn get_storage_before_block(
        &self,
        _block_number: BlockNumber,
        _address: Address,
        _storage_key: B256,
    ) -> ProviderResult<Option<base_execution_state_types::StorageEntry>> {
        Ok(None)
    }

    fn storage_changesets_range(
        &self,
        _range: impl core::ops::RangeBounds<BlockNumber>,
    ) -> ProviderResult<
        Vec<(
            base_execution_state_database::models::BlockNumberAddress,
            base_execution_state_types::StorageEntry,
        )>,
    > {
        Ok(Vec::default())
    }
}

impl StateRootProvider for NoopProvider {
    fn state_root(&self, _state: HashedPostState) -> ProviderResult<B256> {
        Ok(B256::default())
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        Ok(B256::default())
    }

    fn state_root_with_updates(
        &self,
        _state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((B256::default(), TrieUpdates::default()))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((B256::default(), TrieUpdates::default()))
    }
}

impl StorageRootProvider for NoopProvider {
    fn storage_root(
        &self,
        _address: Address,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        Ok(B256::default())
    }

    fn storage_proof(
        &self,
        _address: Address,
        slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        Ok(StorageProof::new(slot))
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        Ok(StorageMultiProof::empty())
    }
}

impl StateProofProvider for NoopProvider {
    fn proof(
        &self,
        _input: TrieInput,
        address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Ok(AccountProof::new(address))
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Ok(MultiProof::default())
    }

    fn witness(
        &self,
        _input: TrieInput,
        _target: HashedPostState,
        _mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        Ok(Vec::default())
    }
}

impl HashedPostStateProvider for NoopProvider {
    fn hashed_post_state(
        &self,
        _bundle_state: &base_execution_state_memory::BundleState,
    ) -> ProviderResult<HashedPostState> {
        Ok(HashedPostState::default())
    }
}

impl StateReader for NoopProvider {
    fn get_state(&self, _block: BlockNumber) -> ProviderResult<Option<ExecutionOutcome>> {
        Ok(None)
    }
}

crate::impl_state_database!([] NoopProvider where []);

impl crate::StateReadProvider for NoopProvider {
    fn storage(
        &self,
        _account: Address,
        _storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        Ok(None)
    }
}

impl BytecodeReader for NoopProvider {
    fn bytecode_by_hash(&self, _code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        Ok(None)
    }
}

impl StateProviderFactory for NoopProvider {
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }

    fn state_by_block_number_or_tag(
        &self,
        number_or_tag: BlockNumberOrTag,
    ) -> ProviderResult<StateProviderBox> {
        match number_or_tag {
            BlockNumberOrTag::Latest => self.latest(),
            BlockNumberOrTag::Finalized => {
                // we can only get the finalized state by hash, not by num
                let hash =
                    self.finalized_block_hash()?.ok_or(ProviderError::FinalizedBlockNotFound)?;

                // only look at historical state
                self.history_by_block_hash(hash)
            }
            BlockNumberOrTag::Safe => {
                // we can only get the safe state by hash, not by num
                let hash = self.safe_block_hash()?.ok_or(ProviderError::SafeBlockNotFound)?;

                self.history_by_block_hash(hash)
            }
            BlockNumberOrTag::Earliest => {
                self.history_by_block_number(self.earliest_block_number()?)
            }
            BlockNumberOrTag::Pending => self.pending(),
            BlockNumberOrTag::Number(num) => self.history_by_block_number(num),
        }
    }

    fn history_by_block_number(&self, _block: BlockNumber) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }

    fn history_by_block_hash(&self, _block: BlockHash) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }

    fn state_by_block_hash(&self, _block: BlockHash) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }

    fn pending(&self) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.clone()))
    }

    fn pending_state_by_hash(&self, _block_hash: B256) -> ProviderResult<Option<StateProviderBox>> {
        Ok(Some(Box::new(self.clone())))
    }

    fn maybe_pending(&self) -> ProviderResult<Option<StateProviderBox>> {
        Ok(Some(Box::new(self.clone())))
    }
}

impl TryIntoHistoricalStateProvider for NoopProvider {
    fn try_into_history_at_block(
        self,
        block_number: BlockNumber,
    ) -> ProviderResult<StateProviderBox> {
        self.history_by_block_number(block_number)
    }
}

impl StageCheckpointReader for NoopProvider {
    fn get_stage_checkpoint(&self, _id: StageId) -> ProviderResult<Option<StageCheckpoint>> {
        Ok(None)
    }

    fn get_stage_checkpoint_progress(&self, _id: StageId) -> ProviderResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn get_all_checkpoints(&self) -> ProviderResult<Vec<(String, StageCheckpoint)>> {
        Ok(Vec::new())
    }
}

impl PruneCheckpointReader for NoopProvider {
    fn get_prune_checkpoint(
        &self,
        _segment: PruneSegment,
    ) -> ProviderResult<Option<PruneCheckpoint>> {
        Ok(None)
    }

    fn get_prune_checkpoints(&self) -> ProviderResult<Vec<(PruneSegment, PruneCheckpoint)>> {
        Ok(Vec::new())
    }
}

impl BlockBodyIndicesProvider for NoopProvider {
    fn block_body_indices(&self, _num: u64) -> ProviderResult<Option<StoredBlockBodyIndices>> {
        Ok(None)
    }

    fn block_body_indices_range(
        &self,
        _range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<StoredBlockBodyIndices>> {
        Ok(Vec::new())
    }
}

#[cfg(feature = "db-api")]
impl DbTxProvider for NoopProvider {
    type Tx = TxMock;

    fn tx(&self) -> &Self::Tx {
        &self.tx
    }
}

#[cfg(feature = "db-api")]
impl DBProvider for NoopProvider {
    fn tx_mut(&mut self) -> &mut Self::Tx {
        &mut self.tx
    }

    fn into_tx(self) -> Self::Tx {
        self.tx
    }

    fn prune_modes_ref(&self) -> &PruneModes {
        &self.prune_modes
    }

    fn commit(self) -> ProviderResult<()> {
        use base_execution_state_database::DbTx;

        Ok(self.tx.commit()?)
    }
}

#[cfg(feature = "db-api")]
impl DatabaseProviderROFactory for NoopProvider {
    type Provider = Self;

    fn database_provider_ro(&self) -> ProviderResult<Self::Provider> {
        Ok(self.clone())
    }
}

#[cfg(feature = "db-api")]
impl DatabaseProviderFactory for NoopProvider {
    type ProviderRW = Self;

    fn database_provider_rw(&self) -> ProviderResult<Self::ProviderRW> {
        Ok(self.clone())
    }
}

#[cfg(feature = "db-api")]
impl StorageSettingsCache for NoopProvider {
    fn cached_storage_settings(&self) -> base_execution_state_database::models::StorageSettings {
        base_execution_state_database::models::StorageSettings::default()
    }

    fn set_storage_settings_cache(
        &self,
        _settings: base_execution_state_database::models::StorageSettings,
    ) {
    }
}
