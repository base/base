//! Transaction pool storage and public operations.

use std::sync::Arc;

use alloy_eips::{
    eip4844::{BlobAndProofV1, BlobAndProofV2, BlobCellsAndProofsV1},
    eip7594::BlobTransactionSidecarVariant,
};
use alloy_primitives::{Address, B128, B256, TxHash, map::AddressSet};
use base_common_chain_config::ChainSpecProvider;
use base_common_types_chain::Recovered;
use base_execution_network_wire::HandleMempoolData;
use base_execution_state_types::{BlockReaderIdExt, ChangedAccount, StateProviderFactory};
use tokio::sync::mpsc::Receiver;
use tracing::{instrument, trace};

use crate::{
    BaseOrdering, BaseTransactionValidator,
    blobstore::{BlobStore, BlobStoreError},
    config::PoolConfig,
    error::PoolResult,
    identifier::TransactionId,
    pool::{
        AddedTransactionOutcome, AllTransactionsEvents, NewTransactionEvent, PoolInner,
        TransactionEvents, TransactionListenerKind,
    },
    traits::*,
    validate::{
        TransactionValidationOutcome, TransactionValidationTaskExecutor, TransactionValidator,
        ValidPoolTransaction,
    },
};

/// A shareable, generic, customizable `TransactionPool` implementation.
#[derive(Debug)]
pub struct Pool<S> {
    /// Arc'ed instance of the pool internals
    pub pool: Arc<PoolInner<S>>,
}

// === impl Pool ===

impl<S> Pool<S>
where
    S: BlobStore,
{
    /// Create a new transaction pool instance.
    pub fn new(
        validator: TransactionValidationTaskExecutor,
        ordering: crate::BaseOrdering,
        blob_store: S,
        config: PoolConfig,
    ) -> Self {
        Self { pool: Arc::new(PoolInner::new(validator, ordering, blob_store, config)) }
    }

    /// Constructs a pool with a validator reserved for test fixtures.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn new_test(
        validator: impl Into<crate::PoolValidator>,
        ordering: crate::BaseOrdering,
        blob_store: S,
        config: PoolConfig,
    ) -> Self {
        Self { pool: Arc::new(PoolInner::new_test(validator, ordering, blob_store, config)) }
    }

    /// Returns the wrapped pool internals.
    pub fn inner(&self) -> &PoolInner<S> {
        &self.pool
    }

    /// Get the config the pool was configured with.
    pub fn config(&self) -> &PoolConfig {
        self.inner().config()
    }

    /// Get the validator reference.
    pub fn validator(&self) -> &crate::PoolValidator {
        self.inner().validator()
    }

    /// Validates the given transaction
    async fn validate(
        &self,
        origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
    ) -> TransactionValidationOutcome {
        self.pool.validator().validate_transaction(origin, transaction).await
    }

    /// Number of transactions in the entire pool
    pub fn len(&self) -> usize {
        self.pool.len()
    }

    /// Whether the pool is empty
    pub fn is_empty(&self) -> bool {
        self.pool.is_empty()
    }

    /// Returns whether or not the pool is over its configured size and transaction count limits.
    pub fn is_exceeded(&self) -> bool {
        self.pool.is_exceeded()
    }

    /// Returns the configured blob store.
    pub fn blob_store(&self) -> &S {
        self.pool.blob_store()
    }
}

impl<S> Pool<S>
where
    S: BlobStore,
{
    /// Returns a new [`Pool`] that uses the default [`TransactionValidationTaskExecutor`] when
    /// validating [`BasePooledTransaction`]s and orders via [`BaseOrdering`]
    ///
    /// # Example
    ///
    /// ```
    ///
    /// use base_execution_state_provider::BlockchainProvider;
    /// use base_common_runtime::Runtime;
    /// use base_execution_txpool::{
    ///     InMemoryBlobStore, Pool, TransactionValidationTaskExecutor,
    /// };
    /// use base_execution_evm_blocks::BaseEvmConfig;
    /// # fn t(client: BlockchainProvider, evm_config: BaseEvmConfig, runtime: Runtime) {
    /// let blob_store = InMemoryBlobStore::default();
    /// let pool = Pool::eth_pool(
    ///     TransactionValidationTaskExecutor::eth(
    ///         client,
    ///         evm_config,
    ///         runtime,
    ///     ),
    ///     blob_store,
    ///     Default::default(),
    /// );
    /// # }
    /// ```
    pub fn eth_pool(
        validator: TransactionValidationTaskExecutor,
        blob_store: S,
        config: PoolConfig,
    ) -> Self {
        Self::new(validator, BaseOrdering::default(), blob_store, config)
    }
}

/// implements the `TransactionPool` interface for various transaction pool API consumers.
impl<S> TransactionPool for Pool<S>
where
    S: BlobStore + Clone,
{
    fn pool_size(&self) -> PoolSize {
        self.pool.size()
    }

    fn block_info(&self) -> BlockInfo {
        self.pool.block_info()
    }

    async fn add_transaction_and_subscribe(
        &self,
        origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
    ) -> PoolResult<TransactionEvents> {
        let tx = self.validate(origin, transaction).await;
        self.pool.add_transaction_and_subscribe(origin, tx)
    }

    async fn add_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
    ) -> PoolResult<AddedTransactionOutcome> {
        let tx = self.validate(origin, transaction).await;
        let mut results = self.pool.add_transactions(origin, std::iter::once(tx));
        results.pop().expect("result length is the same as the input")
    }

    async fn add_transactions(
        &self,
        origin: TransactionOrigin,
        transactions: Vec<crate::BasePooledTransaction>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        if transactions.is_empty() {
            return Vec::new();
        }
        let validated =
            self.pool.validator().validate_transactions_with_origin(origin, transactions).await;
        self.pool.add_transactions(origin, validated)
    }

    async fn add_transactions_with_origins(
        &self,
        transactions: Vec<(TransactionOrigin, crate::BasePooledTransaction)>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        if transactions.is_empty() {
            return Vec::new();
        }
        let origins: Vec<_> = transactions.iter().map(|(origin, _)| *origin).collect();
        let validated = self.pool.validator().validate_transactions(transactions).await;
        self.pool.add_transactions_with_origins(origins.into_iter().zip(validated))
    }

    fn transaction_event_listener(&self, tx_hash: TxHash) -> Option<TransactionEvents> {
        self.pool.add_transaction_event_listener(tx_hash)
    }

    fn all_transactions_event_listener(&self) -> AllTransactionsEvents {
        self.pool.add_all_transactions_event_listener()
    }

    fn pending_transactions_listener_for(&self, kind: TransactionListenerKind) -> Receiver<TxHash> {
        self.pool.add_pending_listener(kind)
    }

    fn blob_transaction_sidecars_listener(&self) -> Receiver<NewBlobSidecar> {
        self.pool.add_blob_sidecar_listener()
    }

    fn new_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> Receiver<NewTransactionEvent> {
        self.pool.add_new_transaction_listener(kind)
    }

    fn pooled_transaction_hashes(&self) -> Vec<TxHash> {
        self.pool.pooled_transactions_hashes()
    }

    fn pooled_transaction_hashes_max(&self, max: usize) -> Vec<TxHash> {
        self.pool.pooled_transactions_hashes_max(max)
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.pooled_transactions()
    }

    fn pooled_transactions_max(&self, max: usize) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.pooled_transactions_max(max)
    }

    fn get_pooled_transaction_elements(
        &self,
        tx_hashes: Vec<TxHash>,
        limit: GetPooledTransactionLimit,
    ) -> Vec<base_common_types_chain::BasePooledTransaction> {
        self.pool.get_pooled_transaction_elements(tx_hashes, limit)
    }

    fn append_pooled_transaction_elements(
        &self,
        tx_hashes: &[TxHash],
        limit: GetPooledTransactionLimit,
        out: &mut Vec<base_common_types_chain::BasePooledTransaction>,
    ) {
        self.pool.append_pooled_transaction_elements(tx_hashes, limit, out)
    }

    fn get_pooled_transaction_element(
        &self,
        tx_hash: TxHash,
    ) -> Option<Recovered<base_common_types_chain::BasePooledTransaction>> {
        self.pool.get_pooled_transaction_element(tx_hash)
    }

    fn best_transactions(&self) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction>>> {
        Box::new(self.pool.best_transactions())
    }

    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction>>> {
        self.pool.best_transactions_with_attributes(best_transactions_attributes)
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.pending_transactions()
    }

    fn get_pending_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction>> {
        self.pool.get_pending_transaction_by_sender_and_nonce(sender, nonce)
    }

    fn pending_transactions_max(&self, max: usize) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.pending_transactions_max(max)
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.queued_transactions()
    }

    fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        let data = self.pool.get_pool_data();
        let pending = data.pending_transactions_count();
        let queued = data.queued_transactions_count();
        (pending, queued)
    }

    fn all_transactions(&self) -> AllPoolTransactions {
        self.pool.all_transactions()
    }

    fn all_transaction_hashes(&self) -> Vec<TxHash> {
        self.pool.all_transaction_hashes()
    }

    fn remove_transactions(&self, hashes: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.remove_transactions(hashes)
    }

    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.remove_transactions_and_descendants(hashes)
    }

    fn remove_transactions_by_sender(&self, sender: Address) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.remove_transactions_by_sender(sender)
    }

    fn prune_transactions(&self, hashes: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.prune_transactions(hashes)
    }

    fn retain_unknown<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData,
    {
        self.pool.retain_unknown(announcement)
    }

    fn retain_contains<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData,
    {
        self.pool.retain_contains(announcement)
    }

    fn get(&self, tx_hash: &TxHash) -> Option<Arc<ValidPoolTransaction>> {
        self.inner().get(tx_hash)
    }

    fn get_all(&self, txs: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction>> {
        self.inner().get_all(txs)
    }

    fn on_propagated(&self, txs: PropagatedTransactions) {
        self.inner().on_propagated(txs)
    }

    fn get_transactions_by_sender(&self, sender: Address) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.get_transactions_by_sender(sender)
    }

    fn get_pending_transactions_with_predicate(
        &self,
        predicate: impl FnMut(&ValidPoolTransaction) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.pending_transactions_with_predicate(predicate)
    }

    fn get_pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.get_pending_transactions_by_sender(sender)
    }

    fn get_queued_transactions_by_sender(&self, sender: Address) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.get_queued_transactions_by_sender(sender)
    }

    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction>> {
        self.pool.get_highest_transaction_by_sender(sender)
    }

    fn get_highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction>> {
        self.pool.get_highest_consecutive_transaction_by_sender(sender, on_chain_nonce)
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction>> {
        let sender_id = self.pool.sender_id(&sender)?;
        let transaction_id = TransactionId::new(sender_id, nonce);

        self.inner().get_pool_data().all().get(&transaction_id).map(|tx| tx.transaction.clone())
    }

    fn get_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.get_transactions_by_origin(origin)
    }

    /// Returns all pending transactions filtered by [`TransactionOrigin`]
    fn get_pending_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction>> {
        self.pool.get_pending_transactions_by_origin(origin)
    }

    fn unique_senders(&self) -> AddressSet {
        self.pool.unique_senders()
    }

    fn get_blob(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        self.pool.blob_store().get(tx_hash)
    }

    fn get_all_blobs(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<(TxHash, Arc<BlobTransactionSidecarVariant>)>, BlobStoreError> {
        self.pool.blob_store().get_all(tx_hashes)
    }

    fn get_all_blobs_exact(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        self.pool.blob_store().get_exact(tx_hashes)
    }

    fn get_blobs_for_versioned_hashes_v1(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV1>>, BlobStoreError> {
        self.pool.blob_store().get_by_versioned_hashes_v1(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v2(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Option<Vec<BlobAndProofV2>>, BlobStoreError> {
        self.pool.blob_store().get_by_versioned_hashes_v2(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v3(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV2>>, BlobStoreError> {
        self.pool.blob_store().get_by_versioned_hashes_v3(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v4(
        &self,
        versioned_hashes: &[B256],
        indices_bitarray: B128,
    ) -> Result<Vec<Option<BlobCellsAndProofsV1>>, BlobStoreError> {
        self.pool.blob_store().get_by_versioned_hashes_v4(versioned_hashes, indices_bitarray)
    }

    fn has_blobs_for_versioned_hashes(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<bool>, BlobStoreError> {
        self.pool.blob_store().has_versioned_hashes(versioned_hashes)
    }

    fn blob_store(&self) -> Box<dyn BlobStore> {
        Box::new(self.pool.blob_store().clone())
    }
}

impl<S> TransactionPoolExt for Pool<S>
where
    S: BlobStore + Clone,
{
    #[instrument(skip(self), target = "txpool")]
    fn set_block_info(&self, info: BlockInfo) {
        trace!(target: "txpool", "updating pool block info");
        self.pool.set_block_info(info)
    }

    fn on_canonical_state_change(&self, update: CanonicalStateUpdate<'_>) {
        self.pool.on_canonical_state_change(update);
    }

    fn update_accounts(&self, accounts: Vec<ChangedAccount>) {
        self.pool.update_accounts(accounts);
    }

    fn delete_blob(&self, tx: TxHash) {
        self.pool.delete_blob(tx)
    }

    fn delete_blobs(&self, txs: Vec<TxHash>) {
        self.pool.delete_blobs(txs)
    }

    fn cleanup_blobs(&self) {
        self.pool.cleanup_blobs()
    }
}

impl<S> ValidatingPool for Pool<S>
where
    S: BlobStore + Clone,
{
    type Validator = crate::PoolValidator;

    fn validator(&self) -> &Self::Validator {
        self.inner().validator()
    }
}

impl<S> Clone for Pool<S> {
    fn clone(&self) -> Self {
        Self { pool: Arc::clone(&self.pool) }
    }
}
