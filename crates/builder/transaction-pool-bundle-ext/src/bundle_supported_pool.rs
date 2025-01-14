//! Houses [`BundleSupportedPool`].

use alloy_eips::eip4844::{BlobAndProofV1, BlobTransactionSidecar};
use alloy_primitives::{Address, TxHash, B256, U256};
use alloy_rpc_types_beacon::events::PayloadAttributesEvent;
use reth::providers::ChangedAccount;
use reth_eth_wire_types::HandleMempoolData;
use reth_primitives::PooledTransactionsElement;
use reth_transaction_pool::{
    AllPoolTransactions, AllTransactionsEvents, BestTransactions, BestTransactionsAttributes,
    BlobStore, BlobStoreError, BlockInfo, CanonicalStateUpdate, EthPoolTransaction,
    GetPooledTransactionLimit, NewBlobSidecar, NewTransactionEvent, Pool, PoolResult, PoolSize,
    PropagatedTransactions, TransactionEvents, TransactionListenerKind, TransactionOrdering,
    TransactionOrigin, TransactionPool, TransactionPoolExt as TransactionPoolBlockInfoExt,
    TransactionValidator, ValidPoolTransaction,
};
use std::{collections::HashSet, future::Future, sync::Arc};
use tokio::sync::mpsc::Receiver;

use crate::{traits::BundlePoolOperations, TransactionPoolBundleExt};

/// Allows easily creating a `Pool` type for reth's `PoolBuilder<Node>` that supports
/// a new [`BundlePool`] running alongside [`Pool`].
///
/// To be used in place of the reth [`Pool`] when defining the `Pool` type
/// for reth's `PoolBuilder<Node>`.
///
/// Just as logic for the [`Pool`] is generic based on `V`, `T`, `S` generics, bundle pool
/// logic is generic based on a new `B` generic that implements [`BundlePoolOperations`].
///
/// Achieves this by implementing [`TransactionPool`] and [`BundlePoolOperations`],
/// and therefore also [`TransactionPoolBundleExt`].
///
/// ## Example
///
/// ```ignore
/// /// An extended optimism transaction pool with bundle support.
/// #[derive(Debug, Default, Clone, Copy)]
/// pub struct CustomPoolBuilder;
///
/// pub type MyCustomTransactionPoolWithBundleSupport<Client, S> = BundleSupportedPool<
///     TransactionValidationTaskExecutor<OpTransactionValidator<Client, EthPooledTransaction>>,
///     CoinbaseTipOrdering<EthPooledTransaction>,
///     S,
///     MyBundlePoolOperationsImplementation,
/// >;
///
/// impl<Node> PoolBuilder<Node> for CustomPoolBuilder
/// where
///     Node: FullNodeTypes,
/// {
///     type Pool = MyCustomTransactionPoolWithBundleSupport<Node::Provider, DiskFileBlobStore>;
///     // the rest of the PoolBuilder implementation...
/// ```
#[derive(Debug)]
pub struct BundleSupportedPool<V, T: TransactionOrdering, S, B> {
    /// Arc'ed instance of [`Pool`] internals
    tx_pool: Pool<V, T, S>,
    /// Arc'ed instance of the [`BundlePool`] internals
    bundle_pool: Arc<BundlePool<B>>,
}

impl<V, T, S, B> BundleSupportedPool<V, T, S, B>
where
    V: TransactionValidator,
    T: TransactionOrdering<Transaction = <V as TransactionValidator>::Transaction>,
    S: BlobStore,
    B: BundlePoolOperations,
{
    pub fn new(pool: Pool<V, T, S>, bundle_ops: B) -> Self {
        Self {
            tx_pool: pool,
            bundle_pool: Arc::new(BundlePool::<B>::new(bundle_ops)),
        }
    }
}

/// Houses generic bundle logic.
#[derive(Debug, Default)]
struct BundlePool<B> {
    pub ops: B,
}

impl<B> BundlePool<B> {
    fn new(ops: B) -> Self {
        Self { ops }
    }
}

/// [`TransactionPool`] requires implementors to be [`Clone`].
impl<V, T: TransactionOrdering, S, B> Clone for BundleSupportedPool<V, T, S, B> {
    fn clone(&self) -> Self {
        Self {
            tx_pool: self.tx_pool.clone(),
            bundle_pool: Arc::clone(&self.bundle_pool),
        }
    }
}

/// Implements the [`TransactionPool`] interface by delegating to the inner `tx_pool`.
/// TODO: Use a crate like `delegate!` or `ambassador` to automate this.
impl<V, T, S, B> TransactionPool for BundleSupportedPool<V, T, S, B>
where
    V: TransactionValidator<Transaction: EthPoolTransaction>,
    T: TransactionOrdering<Transaction = <V as TransactionValidator>::Transaction>,
    S: BlobStore,
    B: BundlePoolOperations,
{
    type Transaction = T::Transaction;

    fn pool_size(&self) -> PoolSize {
        self.tx_pool.pool_size()
    }

    fn block_info(&self) -> BlockInfo {
        self.tx_pool.block_info()
    }

    async fn add_transaction_and_subscribe(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<TransactionEvents> {
        self.tx_pool
            .add_transaction_and_subscribe(origin, transaction)
            .await
    }

    async fn add_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<TxHash> {
        self.tx_pool.add_transaction(origin, transaction).await
    }

    async fn add_transactions(
        &self,
        origin: TransactionOrigin,
        transactions: Vec<Self::Transaction>,
    ) -> Vec<PoolResult<TxHash>> {
        self.tx_pool.add_transactions(origin, transactions).await
    }

    fn transaction_event_listener(&self, tx_hash: TxHash) -> Option<TransactionEvents> {
        self.tx_pool.transaction_event_listener(tx_hash)
    }

    fn all_transactions_event_listener(&self) -> AllTransactionsEvents<Self::Transaction> {
        self.tx_pool.all_transactions_event_listener()
    }

    fn pending_transactions_listener_for(&self, kind: TransactionListenerKind) -> Receiver<TxHash> {
        self.tx_pool.pending_transactions_listener_for(kind)
    }

    fn blob_transaction_sidecars_listener(&self) -> Receiver<NewBlobSidecar> {
        self.tx_pool.blob_transaction_sidecars_listener()
    }

    fn get_pending_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.get_pending_transactions_by_origin(origin)
    }

    fn new_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> Receiver<NewTransactionEvent<Self::Transaction>> {
        self.tx_pool.new_transactions_listener_for(kind)
    }

    fn pooled_transaction_hashes(&self) -> Vec<TxHash> {
        self.tx_pool.pooled_transaction_hashes()
    }

    fn pooled_transaction_hashes_max(&self, max: usize) -> Vec<TxHash> {
        self.tx_pool.pooled_transaction_hashes_max(max)
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.pooled_transactions()
    }

    fn pooled_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.pooled_transactions_max(max)
    }

    fn get_pooled_transaction_elements(
        &self,
        tx_hashes: Vec<TxHash>,
        limit: GetPooledTransactionLimit,
    ) -> Vec<PooledTransactionsElement> {
        self.tx_pool
            .get_pooled_transaction_elements(tx_hashes, limit)
    }

    fn get_pooled_transaction_element(&self, tx_hash: TxHash) -> Option<PooledTransactionsElement> {
        self.tx_pool.get_pooled_transaction_element(tx_hash)
    }

    fn best_transactions(
        &self,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        self.tx_pool.best_transactions()
    }

    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        self.tx_pool
            .best_transactions_with_attributes(best_transactions_attributes)
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.pending_transactions()
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.queued_transactions()
    }

    fn all_transactions(&self) -> AllPoolTransactions<Self::Transaction> {
        self.tx_pool.all_transactions()
    }

    fn remove_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.remove_transactions(hashes)
    }

    fn retain_unknown<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData,
    {
        self.tx_pool.retain_unknown(announcement)
    }

    fn get(&self, tx_hash: &TxHash) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.get(tx_hash)
    }

    fn get_all(&self, txs: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.get_all(txs)
    }

    fn on_propagated(&self, txs: PropagatedTransactions) {
        self.tx_pool.on_propagated(txs)
    }

    fn get_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.get_transactions_by_sender(sender)
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool
            .get_transaction_by_sender_and_nonce(sender, nonce)
    }

    fn get_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.get_transactions_by_origin(origin)
    }

    fn unique_senders(&self) -> HashSet<Address> {
        self.tx_pool.unique_senders()
    }

    fn get_blob(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<Arc<BlobTransactionSidecar>>, BlobStoreError> {
        self.tx_pool.get_blob(tx_hash)
    }

    fn get_all_blobs(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<(TxHash, Arc<BlobTransactionSidecar>)>, BlobStoreError> {
        self.tx_pool.get_all_blobs(tx_hashes)
    }

    fn get_all_blobs_exact(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<Arc<BlobTransactionSidecar>>, BlobStoreError> {
        self.tx_pool.get_all_blobs_exact(tx_hashes)
    }

    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.remove_transactions_and_descendants(hashes)
    }

    fn remove_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.remove_transactions_by_sender(sender)
    }

    fn get_pending_transactions_with_predicate(
        &self,
        predicate: impl FnMut(&ValidPoolTransaction<Self::Transaction>) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool
            .get_pending_transactions_with_predicate(predicate)
    }

    fn get_pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.get_pending_transactions_by_sender(sender)
    }

    fn get_queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.get_queued_transactions_by_sender(sender)
    }

    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool.get_highest_transaction_by_sender(sender)
    }

    fn get_highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.tx_pool
            .get_highest_consecutive_transaction_by_sender(sender, on_chain_nonce)
    }

    fn get_blobs_for_versioned_hashes(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV1>>, BlobStoreError> {
        self.tx_pool
            .get_blobs_for_versioned_hashes(versioned_hashes)
    }
}

/// Implements the [`BundlePoolOperations`] interface by delegating to the inner `bundle_pool`.
/// TODO: Use a crate like `delegate!` or `ambassador` to automate this.
impl<V, T, S, B> BundlePoolOperations for BundleSupportedPool<V, T, S, B>
where
    V: TransactionValidator,
    T: TransactionOrdering<Transaction = <V as TransactionValidator>::Transaction>,
    S: BlobStore,
    B: BundlePoolOperations,
{
    type Bundle = <B as BundlePoolOperations>::Bundle;
    type CancelBundleReq = <B as BundlePoolOperations>::CancelBundleReq;
    type Transaction = <B as BundlePoolOperations>::Transaction;
    type Error = <B as BundlePoolOperations>::Error;

    fn add_bundle(
        &self,
        bundle: Self::Bundle,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.bundle_pool.ops.add_bundle(bundle)
    }

    fn cancel_bundle(
        &self,
        cancel_bundle_req: Self::CancelBundleReq,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.bundle_pool.ops.cancel_bundle(cancel_bundle_req)
    }

    fn get_transactions(
        &self,
        slot: U256,
    ) -> Result<impl IntoIterator<Item = Self::Transaction>, Self::Error> {
        self.bundle_pool.ops.get_transactions(slot)
    }

    fn notify_payload_attributes_event(
        &self,
        payload_attributes: PayloadAttributesEvent,
        gas_limit: Option<u64>,
    ) -> Result<(), Self::Error> {
        self.bundle_pool
            .ops
            .notify_payload_attributes_event(payload_attributes, gas_limit)
    }
}

// Finally, now that [`BundleSupportedPool`] has both [`TransactionPool`] and
// [`BundlePoolOperations`] implemented, it can implement [`TransactionPoolBundleExt`].
impl<V, T, S, B> TransactionPoolBundleExt for BundleSupportedPool<V, T, S, B>
where
    V: TransactionValidator<Transaction: EthPoolTransaction>,
    T: TransactionOrdering<Transaction = <V as TransactionValidator>::Transaction>,
    S: BlobStore,
    B: BundlePoolOperations,
{
}

/// [`TransactionPool`] often requires implementing the block info extension.
impl<V, T, S, B> TransactionPoolBlockInfoExt for BundleSupportedPool<V, T, S, B>
where
    V: TransactionValidator<Transaction: EthPoolTransaction>,
    T: TransactionOrdering<Transaction = <V as TransactionValidator>::Transaction>,
    S: BlobStore,
    B: BundlePoolOperations,
{
    fn set_block_info(&self, info: BlockInfo) {
        self.tx_pool.set_block_info(info)
    }

    fn on_canonical_state_change(&self, update: CanonicalStateUpdate<'_>) {
        self.tx_pool.on_canonical_state_change(update);
    }

    fn update_accounts(&self, accounts: Vec<ChangedAccount>) {
        self.tx_pool.update_accounts(accounts);
    }

    fn delete_blob(&self, tx: TxHash) {
        self.tx_pool.delete_blob(tx)
    }

    fn delete_blobs(&self, txs: Vec<TxHash>) {
        self.tx_pool.delete_blobs(txs)
    }

    fn cleanup_blobs(&self) {
        self.tx_pool.cleanup_blobs()
    }
}
