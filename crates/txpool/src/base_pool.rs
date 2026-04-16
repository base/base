//! Unified transaction pool wrapping Reth's standard pool with the EIP-8130 2D nonce pool.
//!
//! [`BaseTransactionPool`] implements [`TransactionPool`] by delegating to an
//! inner Reth pool for standard transactions while transparently merging
//! results from the [`Eip8130Pool`] side-pool for 2D-nonce AA transactions.
//!
//! This ensures that P2P gossip, RPC queries, and block building all see a
//! consistent view of both standard and 2D-nonce transactions without
//! requiring callers to be aware of the split.

use std::sync::Arc;

use alloy_eips::eip4844::{BlobAndProofV1, BlobAndProofV2};
use alloy_eips::eip7594::BlobTransactionSidecarVariant;
use alloy_primitives::{Address, B256, map::AddressSet};
use reth_eth_wire_types::HandleMempoolData;
use reth_execution_types::ChangedAccount;
use reth_transaction_pool::{
    AllPoolTransactions, BestTransactions, BestTransactionsAttributes, BlockInfo,
    CanonicalStateUpdate, EthPoolTransaction, GetPooledTransactionLimit, NewBlobSidecar,
    PoolResult, PoolSize, PoolTransaction, PropagatedTransactions, TransactionEvents,
    TransactionListenerKind, TransactionOrigin, TransactionPool, TransactionPoolExt,
    ValidPoolTransaction, blobstore::BlobStoreError,
};
use tokio::sync::mpsc::{self, Receiver};

use crate::{MergedBestTransactions, SharedEip8130Pool};

/// Unified transaction pool that wraps a standard Reth pool and the EIP-8130
/// 2D nonce side-pool behind a single [`TransactionPool`] implementation.
///
/// All trait methods that enumerate, query, or fetch transactions check both
/// pools, fixing P2P gossip gaps where the standard pool alone would miss
/// 2D-nonce transactions stored exclusively in the side-pool.
pub struct BaseTransactionPool<P, T> {
    /// Standard Reth pool for non-AA transactions. AA transactions also enter
    /// this pool (for RPC visibility) but are **not** propagated via its
    /// gossip channel — the `Eip8130Pool` drives P2P gossip for all AA txs.
    protocol_pool: P,
    /// Pool for ALL EIP-8130 AA transactions (every `nonce_key` value).
    /// Handles 2D nonce ordering, expiry, invalidation, and P2P gossip.
    eip8130_pool: SharedEip8130Pool<T>,
}

impl<P, T> BaseTransactionPool<P, T> {
    /// Creates a new unified pool wrapping the given protocol pool and EIP-8130 pool.
    pub fn new(protocol_pool: P, eip8130_pool: SharedEip8130Pool<T>) -> Self {
        Self { protocol_pool, eip8130_pool }
    }

    /// Returns a reference to the underlying protocol (Reth) pool.
    pub fn protocol_pool(&self) -> &P {
        &self.protocol_pool
    }

    /// Returns a shared handle to the EIP-8130 2D nonce pool.
    pub fn eip8130_pool(&self) -> SharedEip8130Pool<T> {
        Arc::clone(&self.eip8130_pool)
    }
}

impl<P: Clone, T> Clone for BaseTransactionPool<P, T> {
    fn clone(&self) -> Self {
        Self {
            protocol_pool: self.protocol_pool.clone(),
            eip8130_pool: Arc::clone(&self.eip8130_pool),
        }
    }
}

impl<P: core::fmt::Debug, T> core::fmt::Debug for BaseTransactionPool<P, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BaseTransactionPool")
            .field("protocol_pool", &self.protocol_pool)
            .field("eip8130_pool_len", &self.eip8130_pool.len())
            .finish()
    }
}

impl<P, T> TransactionPool for BaseTransactionPool<P, T>
where
    P: TransactionPool<Transaction = T>,
    T: EthPoolTransaction + Clone,
{
    type Transaction = T;

    fn pool_size(&self) -> PoolSize {
        let mut size = self.protocol_pool.pool_size();
        let (pending, queued) = self.eip8130_pool.pending_and_queued_count();
        size.pending += pending;
        size.queued += queued;
        size
    }

    fn block_info(&self) -> BlockInfo {
        self.protocol_pool.block_info()
    }

    fn add_transaction_and_subscribe(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> impl std::future::Future<Output = PoolResult<TransactionEvents>> + Send {
        self.protocol_pool.add_transaction_and_subscribe(origin, transaction)
    }

    fn add_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> impl std::future::Future<
        Output = PoolResult<reth_transaction_pool::AddedTransactionOutcome>,
    > + Send {
        self.protocol_pool.add_transaction(origin, transaction)
    }

    fn add_transactions_with_origins(
        &self,
        transactions: impl IntoIterator<Item = (TransactionOrigin, Self::Transaction)> + Send,
    ) -> impl std::future::Future<
        Output = Vec<PoolResult<reth_transaction_pool::AddedTransactionOutcome>>,
    > + Send {
        self.protocol_pool.add_transactions_with_origins(transactions)
    }

    fn transaction_event_listener(&self, tx_hash: B256) -> Option<TransactionEvents> {
        self.protocol_pool.transaction_event_listener(tx_hash)
    }

    fn all_transactions_event_listener(
        &self,
    ) -> reth_transaction_pool::AllTransactionsEvents<Self::Transaction> {
        self.protocol_pool.all_transactions_event_listener()
    }

    fn pending_transactions_listener_for(&self, kind: TransactionListenerKind) -> Receiver<B256> {
        let mut proto_rx = self.protocol_pool.pending_transactions_listener_for(kind);
        let mut eip8130_rx = self.eip8130_pool.subscribe_pending_transactions();

        let (merged_tx, merged_rx) = mpsc::channel(512);
        let tx_for_proto = merged_tx.clone();

        tokio::spawn(async move {
            while let Some(hash) = proto_rx.recv().await {
                if tx_for_proto.send(hash).await.is_err() {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            loop {
                match eip8130_rx.recv().await {
                    Ok(hash) => {
                        if merged_tx.send(hash).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        merged_rx
    }

    fn blob_transaction_sidecars_listener(&self) -> Receiver<NewBlobSidecar> {
        self.protocol_pool.blob_transaction_sidecars_listener()
    }

    fn new_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> Receiver<reth_transaction_pool::NewTransactionEvent<Self::Transaction>> {
        self.protocol_pool.new_transactions_listener_for(kind)
    }

    fn pooled_transaction_hashes(&self) -> Vec<B256> {
        let mut hashes = self.protocol_pool.pooled_transaction_hashes();
        hashes.extend(self.eip8130_pool.all_hashes());
        hashes
    }

    fn pooled_transaction_hashes_max(&self, max: usize) -> Vec<B256> {
        let hashes = self.protocol_pool.pooled_transaction_hashes_max(max);
        if hashes.len() >= max {
            return hashes;
        }
        let remaining = max - hashes.len();
        let mut hashes = hashes;
        hashes.extend(self.eip8130_pool.all_hashes().into_iter().take(remaining));
        hashes
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut txs = self.protocol_pool.pooled_transactions();
        txs.extend(self.eip8130_pool.all_transactions());
        txs
    }

    fn pooled_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let txs = self.protocol_pool.pooled_transactions_max(max);
        if txs.len() >= max {
            return txs;
        }
        let remaining = max - txs.len();
        let mut txs = txs;
        txs.extend(self.eip8130_pool.all_transactions().into_iter().take(remaining));
        txs
    }

    fn get_pooled_transaction_elements(
        &self,
        tx_hashes: Vec<B256>,
        limit: GetPooledTransactionLimit,
    ) -> Vec<<Self::Transaction as PoolTransaction>::Pooled> {
        let mut out = Vec::new();
        self.append_pooled_transaction_elements(&tx_hashes, limit, &mut out);
        out
    }

    fn append_pooled_transaction_elements(
        &self,
        tx_hashes: &[B256],
        limit: GetPooledTransactionLimit,
        out: &mut Vec<<Self::Transaction as PoolTransaction>::Pooled>,
    ) {
        // Check the eip8130 pool first for any 2D-nonce transactions.
        let mut accumulated_size = 0;
        for hash in tx_hashes {
            if let Some(tx) = self.eip8130_pool.get(hash) {
                let encoded_len = tx.transaction.encoded_length();
                if let Ok(pooled) = tx.transaction.clone_into_pooled() {
                    accumulated_size += encoded_len;
                    out.push(pooled.into_inner());
                    if limit.exceeds(accumulated_size) {
                        return;
                    }
                }
            }
        }

        // Delegate the rest to the protocol pool (it will skip hashes
        // it doesn't know about, which includes the ones we already served).
        let remaining_limit = match limit {
            GetPooledTransactionLimit::None => GetPooledTransactionLimit::None,
            GetPooledTransactionLimit::ResponseSizeSoftLimit(max) => {
                GetPooledTransactionLimit::ResponseSizeSoftLimit(
                    max.saturating_sub(accumulated_size),
                )
            }
        };
        self.protocol_pool.append_pooled_transaction_elements(tx_hashes, remaining_limit, out);
    }

    fn get_pooled_transaction_element(
        &self,
        tx_hash: B256,
    ) -> Option<reth_primitives_traits::Recovered<<Self::Transaction as PoolTransaction>::Pooled>>
    {
        self.protocol_pool.get_pooled_transaction_element(tx_hash).or_else(|| {
            let tx = self.eip8130_pool.get(&tx_hash)?;
            let pooled = tx.transaction.clone_into_pooled().ok()?;
            Some(pooled)
        })
    }

    fn best_transactions(
        &self,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        let base_fee = self.protocol_pool.block_info().pending_basefee;
        let standard = self.protocol_pool.best_transactions();
        let eip8130 = self.eip8130_pool.best_transactions_with_base_fee(base_fee);
        Box::new(MergedBestTransactions::with_base_fee(standard, eip8130, base_fee))
    }

    fn best_transactions_with_attributes(
        &self,
        attr: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        let standard = self.protocol_pool.best_transactions_with_attributes(attr);
        let eip8130 = self.eip8130_pool.best_transactions_with_base_fee(attr.basefee);
        Box::new(MergedBestTransactions::with_base_fee(standard, eip8130, attr.basefee))
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut pending = self.protocol_pool.pending_transactions();
        pending.extend(self.eip8130_pool.pending_transactions());
        pending
    }

    fn pending_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let txs = self.protocol_pool.pending_transactions_max(max);
        if txs.len() >= max {
            return txs;
        }
        let remaining = max - txs.len();
        let mut txs = txs;
        txs.extend(self.eip8130_pool.pending_transactions().into_iter().take(remaining));
        txs
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut queued = self.protocol_pool.queued_transactions();
        queued.extend(self.eip8130_pool.queued_transactions());
        queued
    }

    fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        let (proto_pending, proto_queued) = self.protocol_pool.pending_and_queued_txn_count();
        let (aa_pending, aa_queued) = self.eip8130_pool.pending_and_queued_count();
        (proto_pending + aa_pending, proto_queued + aa_queued)
    }

    fn all_transactions(&self) -> AllPoolTransactions<Self::Transaction> {
        let mut txs = self.protocol_pool.all_transactions();
        txs.pending.extend(self.eip8130_pool.pending_transactions());
        txs.queued.extend(self.eip8130_pool.queued_transactions());
        txs
    }

    fn all_transaction_hashes(&self) -> Vec<B256> {
        let mut hashes = self.protocol_pool.all_transaction_hashes();
        hashes.extend(self.eip8130_pool.all_hashes());
        hashes
    }

    fn remove_transactions(
        &self,
        hashes: Vec<B256>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.eip8130_pool.remove_transactions(&hashes);
        self.protocol_pool.remove_transactions(hashes)
    }

    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<B256>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.eip8130_pool.remove_transactions(&hashes);
        self.protocol_pool.remove_transactions_and_descendants(hashes)
    }

    fn remove_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let aa_txs = self.eip8130_pool.get_transactions_by_sender(&sender);
        let aa_hashes: Vec<B256> = aa_txs.iter().map(|tx| *tx.hash()).collect();
        if !aa_hashes.is_empty() {
            self.eip8130_pool.remove_transactions(&aa_hashes);
        }
        let mut result = self.protocol_pool.remove_transactions_by_sender(sender);
        result.extend(aa_txs);
        result
    }

    fn prune_transactions(
        &self,
        hashes: Vec<B256>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.eip8130_pool.remove_transactions(&hashes);
        self.protocol_pool.prune_transactions(hashes)
    }

    fn retain_unknown<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData,
    {
        self.protocol_pool.retain_unknown(announcement);
        if announcement.is_empty() {
            return;
        }
        announcement.retain_by_hash(|tx| !self.eip8130_pool.contains(tx));
    }

    fn contains(&self, tx_hash: &B256) -> bool {
        self.protocol_pool.contains(tx_hash) || self.eip8130_pool.contains(tx_hash)
    }

    fn get(&self, tx_hash: &B256) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool.get(tx_hash).or_else(|| self.eip8130_pool.get(tx_hash))
    }

    fn get_all(&self, txs: Vec<B256>) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut result = self.protocol_pool.get_all(txs.clone());
        let found: std::collections::HashSet<B256> = result.iter().map(|tx| *tx.hash()).collect();
        for hash in &txs {
            if !found.contains(hash) {
                if let Some(tx) = self.eip8130_pool.get(hash) {
                    result.push(tx);
                }
            }
        }
        result
    }

    fn on_propagated(&self, txs: PropagatedTransactions) {
        self.protocol_pool.on_propagated(txs);
    }

    fn get_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut txs = self.protocol_pool.get_transactions_by_sender(sender);
        txs.extend(self.eip8130_pool.get_transactions_by_sender(&sender));
        txs
    }

    fn get_pending_transactions_with_predicate(
        &self,
        mut predicate: impl FnMut(&ValidPoolTransaction<Self::Transaction>) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut txs = self.protocol_pool.get_pending_transactions_with_predicate(&mut predicate);
        txs.extend(self.eip8130_pool.pending_transactions().into_iter().filter(|tx| predicate(tx)));
        txs
    }

    fn get_pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut txs = self.protocol_pool.get_pending_transactions_by_sender(sender);
        txs.extend(
            self.eip8130_pool.pending_transactions().into_iter().filter(|tx| tx.sender() == sender),
        );
        txs
    }

    fn get_queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut txs = self.protocol_pool.get_queued_transactions_by_sender(sender);
        txs.extend(
            self.eip8130_pool.queued_transactions().into_iter().filter(|tx| tx.sender() == sender),
        );
        txs
    }

    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        // With 2D nonces there is no single "highest" nonce across all
        // nonce_keys. Return the protocol pool's answer (nonce_key == 0).
        self.protocol_pool.get_highest_transaction_by_sender(sender)
    }

    fn get_highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool.get_highest_consecutive_transaction_by_sender(sender, on_chain_nonce)
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool.get_transaction_by_sender_and_nonce(sender, nonce)
    }

    fn get_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool.get_transactions_by_origin(origin)
    }

    fn get_pending_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool.get_pending_transactions_by_origin(origin)
    }

    fn unique_senders(&self) -> AddressSet {
        let mut senders = self.protocol_pool.unique_senders();
        let inner = self.eip8130_pool.all_transactions();
        for tx in &inner {
            senders.insert(tx.sender());
        }
        senders
    }

    fn get_blob(
        &self,
        tx_hash: B256,
    ) -> Result<Option<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        self.protocol_pool.get_blob(tx_hash)
    }

    fn get_all_blobs(
        &self,
        tx_hashes: Vec<B256>,
    ) -> Result<Vec<(B256, Arc<BlobTransactionSidecarVariant>)>, BlobStoreError> {
        self.protocol_pool.get_all_blobs(tx_hashes)
    }

    fn get_all_blobs_exact(
        &self,
        tx_hashes: Vec<B256>,
    ) -> Result<Vec<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        self.protocol_pool.get_all_blobs_exact(tx_hashes)
    }

    fn get_blobs_for_versioned_hashes_v1(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV1>>, BlobStoreError> {
        self.protocol_pool.get_blobs_for_versioned_hashes_v1(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v2(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Option<Vec<BlobAndProofV2>>, BlobStoreError> {
        self.protocol_pool.get_blobs_for_versioned_hashes_v2(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v3(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV2>>, BlobStoreError> {
        self.protocol_pool.get_blobs_for_versioned_hashes_v3(versioned_hashes)
    }
}

impl<P, T> TransactionPoolExt for BaseTransactionPool<P, T>
where
    P: TransactionPoolExt<Transaction = T>,
    T: EthPoolTransaction + Clone,
{
    type Block = P::Block;

    fn set_block_info(&self, info: BlockInfo) {
        self.protocol_pool.set_block_info(info);
    }

    fn on_canonical_state_change(&self, update: CanonicalStateUpdate<'_, Self::Block>) {
        self.protocol_pool.on_canonical_state_change(update);
    }

    fn update_accounts(&self, accounts: Vec<ChangedAccount>) {
        self.protocol_pool.update_accounts(accounts);
    }

    fn delete_blob(&self, tx: B256) {
        self.protocol_pool.delete_blob(tx);
    }

    fn delete_blobs(&self, txs: Vec<B256>) {
        self.protocol_pool.delete_blobs(txs);
    }

    fn cleanup_blobs(&self) {
        self.protocol_pool.cleanup_blobs();
    }
}
