//! Node-level transaction pool that is either the full Base pool or a disabled pool.

use std::{fmt, sync::Arc};

use alloy_eips::{
    eip4844::{BlobAndProofV1, BlobAndProofV2, BlobCellsAndProofsV1},
    eip7594::BlobTransactionSidecarVariant,
};
use alloy_primitives::{Address, B128, B256, TxHash, map::AddressSet};
use reth_eth_wire_types::HandleMempoolData;
use reth_execution_types::ChangedAccount;
use reth_primitives_traits::Recovered;
use reth_transaction_pool::{
    AddedTransactionOutcome, AllPoolTransactions, AllTransactionsEvents, BestTransactions,
    BestTransactionsAttributes, BlobStore, BlobStoreError, BlockInfo, CanonicalStateUpdate,
    GetPooledTransactionLimit, NewBlobSidecar, NewTransactionEvent, PoolResult, PoolSize,
    PoolTransaction, PropagatedTransactions, TransactionEvents, TransactionListenerKind,
    TransactionOrdering, TransactionOrigin, TransactionPool, TransactionPoolExt,
    TransactionValidator, ValidPoolTransaction, noop::NoopTransactionPool,
};
use tokio::sync::mpsc;

use crate::{
    AccountStateDiff, BasePooledTx, BaseTransactionPool, BaseTransactionValidator,
    InvalidationCause, ParkableBestTransactions, ParkableTransactionPool, ParkedBestTransactions,
    StateDiffInvalidation,
};

/// Forwards a call to whichever pool backs this [`MaybeBaseTransactionPool`].
macro_rules! dispatch {
    ($self:expr, $pool:ident => $call:expr) => {
        match $self {
            Self::Enabled($pool) => $call,
            Self::Disabled { pool: $pool, .. } => $call,
        }
    };
}

/// The transaction pool a Base node runs with.
///
/// Nodes that never build blocks and never drain the pool (plain RPC and follower nodes) run
/// [`Self::Disabled`], which rejects every insert and holds nothing. Sequencers, builders and
/// transaction forwarders run [`Self::Enabled`]. Sharing one type keeps every component and
/// extension that consumes the pool independent of which mode was chosen at startup.
pub enum MaybeBaseTransactionPool<Client, S, Evm, T, O>
where
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
    O: TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    /// The full Base transaction pool.
    Enabled(BaseTransactionPool<Client, S, Evm, T, O>),
    /// A pool that rejects all transactions.
    Disabled {
        /// Reth's noop pool, which rejects every insert.
        pool: NoopTransactionPool<T>,
        /// Ordering used to build empty parkable iterators.
        ordering: O,
    },
}

impl<Client, S, Evm, T, O> fmt::Debug for MaybeBaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
    O: TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = if self.is_enabled() { "Enabled" } else { "Disabled" };
        f.debug_struct("MaybeBaseTransactionPool")
            .field("variant", &variant)
            .finish_non_exhaustive()
    }
}

impl<Client, S, Evm, T, O> Clone for MaybeBaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
    O: TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    fn clone(&self) -> Self {
        match self {
            Self::Enabled(pool) => Self::Enabled(pool.clone()),
            Self::Disabled { pool, ordering } => {
                Self::Disabled { pool: pool.clone(), ordering: ordering.clone() }
            }
        }
    }
}

impl<Client, S, Evm, T, O> Unpin for MaybeBaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
    O: TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
}

impl<Client, S, Evm, T, O> MaybeBaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
    O: TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    /// Creates a disabled pool that rejects every transaction.
    pub fn disabled(ordering: O) -> Self {
        Self::Disabled { pool: NoopTransactionPool::new(), ordering }
    }

    /// Returns `true` if this is the full pool rather than one that rejects every transaction.
    pub const fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled(_))
    }
}

impl<Client, S, Evm, T, O> StateDiffInvalidation for MaybeBaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    fn invalidate_from_state_diff(&self, diffs: &[AccountStateDiff]) -> usize {
        match self {
            Self::Enabled(pool) => pool.invalidate_from_state_diff(diffs),
            Self::Disabled { .. } => 0,
        }
    }

    fn invalidate_all_tracked(&self, cause: InvalidationCause) -> usize {
        match self {
            Self::Enabled(pool) => pool.invalidate_all_tracked(cause),
            Self::Disabled { .. } => 0,
        }
    }
}

impl<Client, S, Evm, T, O> ParkableTransactionPool
    for MaybeBaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: TransactionOrdering<Transaction = T> + Clone + 'static,
    S: BlobStore + Clone,
{
    fn best_transactions_with_attributes_and_parking(
        &self,
        attributes: BestTransactionsAttributes,
    ) -> Box<dyn ParkableBestTransactions<Self::Transaction>> {
        match self {
            Self::Enabled(pool) => pool.best_transactions_with_attributes_and_parking(attributes),
            Self::Disabled { ordering, .. } => {
                let empty: Box<
                    dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>,
                > = Box::new(std::iter::empty());
                Box::new(ParkedBestTransactions::new(empty, ordering.clone(), attributes.basefee))
            }
        }
    }
}

impl<Client, S, Evm, T, O> TransactionPoolExt for MaybeBaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    type Block = <BaseTransactionPool<Client, S, Evm, T, O> as TransactionPoolExt>::Block;

    fn set_block_info(&self, info: BlockInfo) {
        if let Self::Enabled(pool) = self {
            pool.set_block_info(info);
        }
    }

    fn on_canonical_state_change(&self, update: CanonicalStateUpdate<'_, Self::Block>) {
        if let Self::Enabled(pool) = self {
            pool.on_canonical_state_change(update);
        }
    }

    fn update_accounts(&self, accounts: Vec<ChangedAccount>) {
        if let Self::Enabled(pool) = self {
            pool.update_accounts(accounts);
        }
    }

    fn delete_blob(&self, tx: B256) {
        if let Self::Enabled(pool) = self {
            pool.delete_blob(tx);
        }
    }

    fn delete_blobs(&self, txs: Vec<B256>) {
        if let Self::Enabled(pool) = self {
            pool.delete_blobs(txs);
        }
    }

    fn cleanup_blobs(&self) {
        if let Self::Enabled(pool) = self {
            pool.cleanup_blobs();
        }
    }
}

impl<Client, S, Evm, T, O> TransactionPool for MaybeBaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    type Transaction = T;

    fn pool_size(&self) -> PoolSize {
        dispatch!(self, pool => pool.pool_size())
    }

    fn block_info(&self) -> BlockInfo {
        dispatch!(self, pool => pool.block_info())
    }

    async fn add_transaction_and_subscribe(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<TransactionEvents> {
        dispatch!(self, pool => pool.add_transaction_and_subscribe(origin, transaction).await)
    }

    async fn add_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<AddedTransactionOutcome> {
        dispatch!(self, pool => pool.add_transaction(origin, transaction).await)
    }

    async fn add_transactions(
        &self,
        origin: TransactionOrigin,
        transactions: Vec<Self::Transaction>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        dispatch!(self, pool => pool.add_transactions(origin, transactions).await)
    }

    async fn add_transactions_with_origins(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        dispatch!(self, pool => pool.add_transactions_with_origins(transactions).await)
    }

    fn transaction_event_listener(&self, tx_hash: TxHash) -> Option<TransactionEvents> {
        dispatch!(self, pool => pool.transaction_event_listener(tx_hash))
    }

    fn all_transactions_event_listener(&self) -> AllTransactionsEvents<Self::Transaction> {
        dispatch!(self, pool => pool.all_transactions_event_listener())
    }

    fn pending_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<TxHash> {
        dispatch!(self, pool => pool.pending_transactions_listener_for(kind))
    }

    fn new_transactions_listener(&self) -> mpsc::Receiver<NewTransactionEvent<Self::Transaction>> {
        dispatch!(self, pool => pool.new_transactions_listener())
    }

    fn blob_transaction_sidecars_listener(&self) -> mpsc::Receiver<NewBlobSidecar> {
        dispatch!(self, pool => pool.blob_transaction_sidecars_listener())
    }

    fn new_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<NewTransactionEvent<Self::Transaction>> {
        dispatch!(self, pool => pool.new_transactions_listener_for(kind))
    }

    fn pooled_transaction_hashes(&self) -> Vec<TxHash> {
        dispatch!(self, pool => pool.pooled_transaction_hashes())
    }

    fn pooled_transaction_hashes_max(&self, max: usize) -> Vec<TxHash> {
        dispatch!(self, pool => pool.pooled_transaction_hashes_max(max))
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.pooled_transactions())
    }

    fn pooled_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.pooled_transactions_max(max))
    }

    fn get_pooled_transaction_elements(
        &self,
        tx_hashes: Vec<TxHash>,
        limit: GetPooledTransactionLimit,
    ) -> Vec<<Self::Transaction as PoolTransaction>::Pooled> {
        dispatch!(self, pool => pool.get_pooled_transaction_elements(tx_hashes, limit))
    }

    fn append_pooled_transaction_elements(
        &self,
        tx_hashes: &[TxHash],
        limit: GetPooledTransactionLimit,
        out: &mut Vec<<Self::Transaction as PoolTransaction>::Pooled>,
    ) {
        dispatch!(self, pool => pool.append_pooled_transaction_elements(tx_hashes, limit, out))
    }

    fn get_pooled_transaction_element(
        &self,
        tx_hash: TxHash,
    ) -> Option<Recovered<<Self::Transaction as PoolTransaction>::Pooled>> {
        dispatch!(self, pool => pool.get_pooled_transaction_element(tx_hash))
    }

    fn best_transactions(
        &self,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        dispatch!(self, pool => pool.best_transactions())
    }

    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        dispatch!(self, pool => pool.best_transactions_with_attributes(best_transactions_attributes))
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.pending_transactions())
    }

    fn pending_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.pending_transactions_max(max))
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.queued_transactions())
    }

    fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        dispatch!(self, pool => pool.pending_and_queued_txn_count())
    }

    fn all_transactions(&self) -> AllPoolTransactions<Self::Transaction> {
        dispatch!(self, pool => pool.all_transactions())
    }

    fn all_transaction_hashes(&self) -> Vec<TxHash> {
        dispatch!(self, pool => pool.all_transaction_hashes())
    }

    fn remove_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.remove_transactions(hashes))
    }

    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.remove_transactions_and_descendants(hashes))
    }

    fn remove_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.remove_transactions_by_sender(sender))
    }

    fn prune_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.prune_transactions(hashes))
    }

    fn retain_unknown<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData,
    {
        dispatch!(self, pool => pool.retain_unknown(announcement))
    }

    fn retain_contains<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData,
    {
        dispatch!(self, pool => pool.retain_contains(announcement))
    }

    fn get(&self, tx_hash: &TxHash) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.get(tx_hash))
    }

    fn get_all(&self, txs: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.get_all(txs))
    }

    fn on_propagated(&self, txs: PropagatedTransactions) {
        dispatch!(self, pool => pool.on_propagated(txs))
    }

    fn get_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.get_transactions_by_sender(sender))
    }

    fn get_pending_transactions_with_predicate(
        &self,
        predicate: impl FnMut(&ValidPoolTransaction<Self::Transaction>) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.get_pending_transactions_with_predicate(predicate))
    }

    fn get_pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.get_pending_transactions_by_sender(sender))
    }

    fn get_queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.get_queued_transactions_by_sender(sender))
    }

    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.get_highest_transaction_by_sender(sender))
    }

    fn get_highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.get_highest_consecutive_transaction_by_sender(sender, on_chain_nonce))
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.get_transaction_by_sender_and_nonce(sender, nonce))
    }

    fn get_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.get_transactions_by_origin(origin))
    }

    fn get_pending_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        dispatch!(self, pool => pool.get_pending_transactions_by_origin(origin))
    }

    fn unique_senders(&self) -> AddressSet {
        dispatch!(self, pool => pool.unique_senders())
    }

    fn get_blob(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        dispatch!(self, pool => pool.get_blob(tx_hash))
    }

    fn get_all_blobs(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<(TxHash, Arc<BlobTransactionSidecarVariant>)>, BlobStoreError> {
        dispatch!(self, pool => pool.get_all_blobs(tx_hashes))
    }

    fn get_all_blobs_exact(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        dispatch!(self, pool => pool.get_all_blobs_exact(tx_hashes))
    }

    fn get_blobs_for_versioned_hashes_v1(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV1>>, BlobStoreError> {
        dispatch!(self, pool => pool.get_blobs_for_versioned_hashes_v1(versioned_hashes))
    }

    fn get_blobs_for_versioned_hashes_v2(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Option<Vec<BlobAndProofV2>>, BlobStoreError> {
        dispatch!(self, pool => pool.get_blobs_for_versioned_hashes_v2(versioned_hashes))
    }

    fn get_blobs_for_versioned_hashes_v3(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV2>>, BlobStoreError> {
        dispatch!(self, pool => pool.get_blobs_for_versioned_hashes_v3(versioned_hashes))
    }

    fn get_blobs_for_versioned_hashes_v4(
        &self,
        versioned_hashes: &[B256],
        indices_bitarray: B128,
    ) -> Result<Vec<Option<BlobCellsAndProofsV1>>, BlobStoreError> {
        dispatch!(self, pool => pool.get_blobs_for_versioned_hashes_v4(versioned_hashes, indices_bitarray))
    }

    fn has_blobs_for_versioned_hashes(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<bool>, BlobStoreError> {
        dispatch!(self, pool => pool.has_blobs_for_versioned_hashes(versioned_hashes))
    }

    fn blob_store(&self) -> Box<dyn BlobStore> {
        dispatch!(self, pool => pool.blob_store())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::TxHash;
    use base_common_consensus::BasePrimitives;
    use base_execution_chainspec::BaseChainSpec;
    use base_execution_evm::BaseEvmConfig;
    use reth_provider::test_utils::MockEthProvider;
    use reth_transaction_pool::{TransactionPool, blobstore::InMemoryBlobStore};

    use super::*;
    use crate::{BaseOrdering, BasePooledTransaction};

    type TestPool = MaybeBaseTransactionPool<
        MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
        InMemoryBlobStore,
        BaseEvmConfig,
        BasePooledTransaction,
        BaseOrdering<BasePooledTransaction>,
    >;

    #[test]
    fn disabled_pool_holds_nothing() {
        let pool = TestPool::disabled(BaseOrdering::default());

        assert!(!pool.is_enabled());
        assert_eq!(pool.pool_size().total, 0);
        assert!(pool.pending_transactions().is_empty());
        assert!(pool.get(&TxHash::ZERO).is_none());
        assert!(
            pool.best_transactions_with_attributes_and_parking(
                BestTransactionsAttributes::base_fee(0)
            )
            .next()
            .is_none()
        );
    }
}
