//! Transaction Pool Traits and Types
//!
//! This module defines the core abstractions for transaction pool implementations,
//! handling the complexity of different transaction representations across the
//! network, mempool, and the chain itself.
//!
//! ## Key Concepts
//!
//! ### Transaction Representations
//!
//! Transactions exist in different formats throughout their lifecycle:
//!
//! 1. **Consensus Format** ([`BasePooledTransaction::Consensus`])
//!    - The canonical format stored in blocks
//!    - Minimal size for efficient storage
//!    - Example: EIP-4844 transactions store only blob hashes: ([`EthereumTxEnvelope::<TxEip4844>::Eip4844`])
//!
//! 2. **Pooled Format** ([`BasePooledTransaction::Pooled`])
//!    - Extended format for network propagation
//!    - Includes additional validation data
//!    - Example: EIP-4844 transactions include full blob sidecars: ([`EthereumTxEnvelope::<TxEip4844WithSidecar<alloy_eips::eip7594::BlobTransactionSidecarVariant>,>`])
//!
//! ### Type Relationships
//!
//! ```text
//! BaseTxEnvelope  ←──   BaseTxEnvelope (broadcast)
//!        │                              │
//!        │ (consensus format)           │ (announced to peers)
//!        │                              │
//!        └──────────┐  ┌────────────────┘
//!                   ▼  ▼
//!            BasePooledTransaction::Consensus
//!                   │ ▲
//!                   │ │ from pooled (always succeeds)
//!                   │ │
//!                   ▼ │ try_from consensus (may fail)
//!            BasePooledTransaction::Pooled  ←──→  BasePooledTransaction (wire)
//!                                             (sent on request)
//! ```
//!
//! ### Special Cases
//!
//! #### EIP-4844 Blob Transactions
//! - Consensus format: Only blob hashes (32 bytes each)
//! - Pooled format: Full blobs + commitments + proofs (large data per blob)
//! - Network behavior: Not broadcast automatically, only sent on explicit request
//!
//! #### Optimism Deposit Transactions
//! - Only exist in consensus format
//! - Never enter the mempool (system transactions)
//! - Conversion from consensus to pooled always fails

use std::{
    fmt,
    fmt::Debug,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use alloy_eips::{
    eip4844::{BlobAndProofV1, BlobAndProofV2, BlobCellsAndProofsV1},
    eip7594::BlobTransactionSidecarVariant,
};
use alloy_primitives::{
    Address, B128, B256, TxHash,
    map::{AddressSet, B256Map},
};
use base_common_types_chain::{BlockHeader, transaction::TxHashRef};
use base_execution_network_wire::HandleMempoolData;
use base_execution_state_types::ChangedAccount;
use futures_util::{Stream, ready};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Receiver;
use {base_common_types_chain::Recovered, base_common_types_chain::SealedBlock};

use crate::{
    AddedTransactionOutcome, AllTransactionsEvents, SubPool,
    blobstore::{BlobStore, BlobStoreError, PooledBlobSidecar},
    error::{InvalidPoolTransactionError, PoolError, PoolResult},
    pool::{
        BestTransactionFilter, NewTransactionEvent, TransactionEvents, TransactionListenerKind,
    },
    validate::{TransactionValidationOutcome, TransactionValidator, ValidPoolTransaction},
};

/// The `PeerId` type.
pub type PeerId = alloy_primitives::B512;

/// Cached Base transaction held by the pool.
pub type PoolTx = crate::BasePooledTransaction;
/// Base transaction envelope stored in blocks.
pub type PoolConsensusTx = base_common_types_chain::BaseTxEnvelope;

/// Base transaction envelope admitted to the pool.
pub type PoolPooledTx = base_common_types_chain::BasePooledTransaction;

/// General purpose abstraction of a transaction-pool.
///
/// This is intended to be used by API-consumers such as RPC that need inject new incoming,
/// unverified transactions. And by block production that needs to get transactions to execute in a
/// new block.
///
/// Note: This requires `Clone` for convenience, since it is assumed that this will be implemented
/// for a wrapped `Arc` type, see also [`Pool`](crate::Pool).
#[auto_impl::auto_impl(&, Arc)]
pub trait TransactionPool: Clone + Debug + Send + Sync {
    /// Returns stats about the pool and all sub-pools.
    fn pool_size(&self) -> PoolSize;

    /// Returns the block the pool is currently tracking.
    ///
    /// This tracks the block that the pool has last seen.
    fn block_info(&self) -> BlockInfo;

    /// Imports an _external_ transaction.
    ///
    /// This is intended to be used by the network to insert incoming transactions received over the
    /// p2p network.
    ///
    /// Consumer: P2P
    fn add_external_transaction(
        &self,
        transaction: crate::BasePooledTransaction,
    ) -> impl Future<Output = PoolResult<AddedTransactionOutcome>> + Send {
        self.add_transaction(TransactionOrigin::External, transaction)
    }

    /// Imports all _external_ transactions
    ///
    /// Consumer: Utility
    fn add_external_transactions(
        &self,
        transactions: Vec<crate::BasePooledTransaction>,
    ) -> impl Future<Output = Vec<PoolResult<AddedTransactionOutcome>>> + Send {
        self.add_transactions(TransactionOrigin::External, transactions)
    }

    /// Adds an _unvalidated_ transaction into the pool and subscribe to state changes.
    ///
    /// This is the same as [`TransactionPool::add_transaction`] but returns an event stream for the
    /// given transaction.
    ///
    /// Consumer: Custom
    fn add_transaction_and_subscribe(
        &self,
        origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
    ) -> impl Future<Output = PoolResult<TransactionEvents>> + Send;

    /// Adds an _unvalidated_ transaction into the pool.
    ///
    /// Consumer: RPC
    fn add_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
    ) -> impl Future<Output = PoolResult<AddedTransactionOutcome>> + Send;

    /// Adds the given _unvalidated_ transactions into the pool.
    ///
    /// All transactions will use the same `origin`.
    ///
    /// Returns a list of results.
    ///
    /// Consumer: RPC
    fn add_transactions(
        &self,
        origin: TransactionOrigin,
        transactions: Vec<crate::BasePooledTransaction>,
    ) -> impl Future<Output = Vec<PoolResult<AddedTransactionOutcome>>> + Send;

    /// Adds the given _unvalidated_ transactions into the pool.
    ///
    /// Each transaction is paired with its own [`TransactionOrigin`].
    ///
    /// Returns a list of results.
    ///
    /// Consumer: RPC
    fn add_transactions_with_origins(
        &self,
        transactions: Vec<(TransactionOrigin, crate::BasePooledTransaction)>,
    ) -> impl Future<Output = Vec<PoolResult<AddedTransactionOutcome>>> + Send;

    /// Submit a consensus transaction directly to the pool
    fn add_consensus_transaction(
        &self,
        tx: Recovered<base_common_types_chain::BaseTxEnvelope>,
        origin: TransactionOrigin,
    ) -> impl Future<Output = PoolResult<AddedTransactionOutcome>> + Send {
        async move {
            let tx_hash = *tx.tx_hash();

            let pool_transaction = match crate::BasePooledTransaction::try_from_consensus(tx) {
                Ok(tx) => tx,
                Err(e) => return Err(PoolError::other(tx_hash, e.to_string())),
            };

            self.add_transaction(origin, pool_transaction).await
        }
    }

    /// Submit a consensus transaction and subscribe to event stream
    fn add_consensus_transaction_and_subscribe(
        &self,
        tx: Recovered<base_common_types_chain::BaseTxEnvelope>,
        origin: TransactionOrigin,
    ) -> impl Future<Output = PoolResult<TransactionEvents>> + Send {
        async move {
            let tx_hash = *tx.tx_hash();

            let pool_transaction = match crate::BasePooledTransaction::try_from_consensus(tx) {
                Ok(tx) => tx,
                Err(e) => return Err(PoolError::other(tx_hash, e.to_string())),
            };

            self.add_transaction_and_subscribe(origin, pool_transaction).await
        }
    }

    /// Returns a new transaction change event stream for the given transaction.
    ///
    /// Returns `None` if the transaction is not in the pool.
    fn transaction_event_listener(&self, tx_hash: TxHash) -> Option<TransactionEvents>;

    /// Returns a new transaction change event stream for _all_ transactions in the pool.
    fn all_transactions_event_listener(&self) -> AllTransactionsEvents;

    /// Returns a new Stream that yields transactions hashes for new __pending__ transactions
    /// inserted into the pool that are allowed to be propagated.
    ///
    /// Note: This is intended for networking and will __only__ yield transactions that are allowed
    /// to be propagated over the network, see also [`TransactionListenerKind`].
    ///
    /// Consumer: RPC/P2P
    fn pending_transactions_listener(&self) -> Receiver<TxHash> {
        self.pending_transactions_listener_for(TransactionListenerKind::PropagateOnly)
    }

    /// Returns a new [Receiver] that yields transactions hashes for new __pending__ transactions
    /// inserted into the pending pool depending on the given [`TransactionListenerKind`] argument.
    fn pending_transactions_listener_for(&self, kind: TransactionListenerKind) -> Receiver<TxHash>;

    /// Returns a new stream that yields new valid transactions added to the pool.
    fn new_transactions_listener(&self) -> Receiver<NewTransactionEvent> {
        self.new_transactions_listener_for(TransactionListenerKind::PropagateOnly)
    }

    /// Returns a new [Receiver] that yields blob "sidecars" (blobs w/ assoc. kzg
    /// commitments/proofs) for eip-4844 transactions inserted into the pool
    fn blob_transaction_sidecars_listener(&self) -> Receiver<NewBlobSidecar>;

    /// Returns a new stream that yields new valid transactions added to the pool
    /// depending on the given [`TransactionListenerKind`] argument.
    fn new_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> Receiver<NewTransactionEvent>;

    /// Returns a new Stream that yields new transactions added to the pending sub-pool.
    ///
    /// This is a convenience wrapper around [`Self::new_transactions_listener`] that filters for
    /// [`SubPool::Pending`](crate::SubPool).
    fn new_pending_pool_transactions_listener(&self) -> NewSubpoolTransactionStream {
        NewSubpoolTransactionStream::new(
            self.new_transactions_listener_for(TransactionListenerKind::PropagateOnly),
            SubPool::Pending,
        )
    }

    /// Returns a new Stream that yields new transactions added to the basefee sub-pool.
    ///
    /// This is a convenience wrapper around [`Self::new_transactions_listener`] that filters for
    /// [`SubPool::BaseFee`](crate::SubPool).
    fn new_basefee_pool_transactions_listener(&self) -> NewSubpoolTransactionStream {
        NewSubpoolTransactionStream::new(self.new_transactions_listener(), SubPool::BaseFee)
    }

    /// Returns a new Stream that yields new transactions added to the queued-pool.
    ///
    /// This is a convenience wrapper around [`Self::new_transactions_listener`] that filters for
    /// [`SubPool::Queued`](crate::SubPool).
    fn new_queued_transactions_listener(&self) -> NewSubpoolTransactionStream {
        NewSubpoolTransactionStream::new(self.new_transactions_listener(), SubPool::Queued)
    }

    /// Returns a new Stream that yields new transactions added to the blob sub-pool.
    ///
    /// This is a convenience wrapper around [`Self::new_transactions_listener`] that filters for
    /// [`SubPool::Blob`](crate::SubPool).
    fn new_blob_pool_transactions_listener(&self) -> NewSubpoolTransactionStream {
        NewSubpoolTransactionStream::new(self.new_transactions_listener(), SubPool::Blob)
    }

    /// Returns the _hashes_ of all transactions in the pool that are allowed to be propagated.
    ///
    /// This excludes hashes that aren't allowed to be propagated.
    ///
    /// Note: This returns a `Vec` but should guarantee that all hashes are unique.
    ///
    /// Consumer: P2P
    fn pooled_transaction_hashes(&self) -> Vec<TxHash>;

    /// Returns only the first `max` hashes of transactions in the pool.
    ///
    /// Consumer: P2P
    fn pooled_transaction_hashes_max(&self, max: usize) -> Vec<TxHash>;

    /// Returns the _full_ transaction objects all transactions in the pool that are allowed to be
    /// propagated.
    ///
    /// This is intended to be used by the network for the initial exchange of pooled transaction
    /// _hashes_
    ///
    /// Note: This returns a `Vec` but should guarantee that all transactions are unique.
    ///
    /// Caution: In case of blob transactions, this does not include the sidecar.
    ///
    /// Consumer: P2P
    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction>>;

    /// Returns only the first `max` transactions in the pool.
    ///
    /// Consumer: P2P
    fn pooled_transactions_max(&self, max: usize) -> Vec<Arc<ValidPoolTransaction>>;

    /// Returns converted [`EthereumTxEnvelope::<TxEip4844WithSidecar<alloy_eips::eip7594::BlobTransactionSidecarVariant>,>`] for the given transaction hashes that are
    /// allowed to be propagated.
    ///
    /// This adheres to the expected behavior of
    /// [`GetPooledTransactions`](https://github.com/ethereum/devp2p/blob/master/caps/eth.md#getpooledtransactions-0x09):
    ///
    /// The transactions must be in same order as in the request, but it is OK to skip transactions
    /// which are not available.
    ///
    /// If the transaction is a blob transaction, the sidecar will be included.
    ///
    /// Consumer: P2P
    fn get_pooled_transaction_elements(
        &self,
        tx_hashes: Vec<TxHash>,
        limit: GetPooledTransactionLimit,
    ) -> Vec<base_common_types_chain::BasePooledTransaction>;

    /// Extends the given vector with pooled transactions for the given hashes that are allowed to
    /// be propagated.
    ///
    /// This adheres to the expected behavior of [`Self::get_pooled_transaction_elements`].
    ///
    /// Consumer: P2P
    fn append_pooled_transaction_elements(
        &self,
        tx_hashes: &[TxHash],
        limit: GetPooledTransactionLimit,
        out: &mut Vec<base_common_types_chain::BasePooledTransaction>,
    ) {
        out.extend(self.get_pooled_transaction_elements(tx_hashes.to_vec(), limit));
    }

    /// Returns the pooled transaction variant for the given transaction hash.
    ///
    /// This adheres to the expected behavior of
    /// [`GetPooledTransactions`](https://github.com/ethereum/devp2p/blob/master/caps/eth.md#getpooledtransactions-0x09):
    ///
    /// If the transaction is a blob transaction, the sidecar will be included.
    ///
    /// It is expected that this variant represents the valid p2p format for full transactions.
    /// E.g. for EIP-4844 transactions this is the consensus transaction format with the blob
    /// sidecar.
    ///
    /// Consumer: P2P
    fn get_pooled_transaction_element(
        &self,
        tx_hash: TxHash,
    ) -> Option<Recovered<base_common_types_chain::BasePooledTransaction>>;

    /// Returns an iterator that yields transactions that are ready for block production.
    ///
    /// Consumer: Block production
    fn best_transactions(&self) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction>>>;

    /// Returns an iterator that yields transactions that are ready for block production with the
    /// given base fee and optional blob fee attributes.
    ///
    /// Consumer: Block production
    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction>>>;

    /// Returns all transactions that can be included in the next block.
    ///
    /// This is primarily used for the `txpool_` RPC namespace:
    /// <https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-txpool> which distinguishes
    /// between `pending` and `queued` transactions, where `pending` are transactions ready for
    /// inclusion in the next block and `queued` are transactions that are ready for inclusion in
    /// future blocks.
    ///
    /// Consumer: RPC
    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction>>;

    /// Returns a pending transaction if it exists and is ready for immediate execution
    /// (i.e., has the lowest nonce among the sender's pending transactions).
    fn get_pending_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction>> {
        self.best_transactions().find(|tx| tx.sender() == sender && tx.nonce() == nonce)
    }

    /// Returns first `max` transactions that can be included in the next block.
    /// See <https://github.com/paradigmxyz/reth/issues/12767#issuecomment-2493223579>
    ///
    /// Consumer: Block production
    fn pending_transactions_max(&self, max: usize) -> Vec<Arc<ValidPoolTransaction>>;

    /// Returns all transactions that can be included in _future_ blocks.
    ///
    /// This and [`Self::pending_transactions`] are mutually exclusive.
    ///
    /// Consumer: RPC
    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction>>;

    /// Returns the number of transactions that are ready for inclusion in the next block and the
    /// number of transactions that are ready for inclusion in future blocks: `(pending, queued)`.
    fn pending_and_queued_txn_count(&self) -> (usize, usize);

    /// Returns all transactions that are currently in the pool grouped by whether they are ready
    /// for inclusion in the next block or not.
    ///
    /// This is primarily used for the `txpool_` namespace: <https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-txpool>
    ///
    /// Consumer: RPC
    fn all_transactions(&self) -> AllPoolTransactions;

    /// Returns the _hashes_ of all transactions regardless of whether they can be propagated or
    /// not.
    ///
    /// Unlike [`Self::pooled_transaction_hashes`] this doesn't consider whether the transaction can
    /// be propagated or not.
    ///
    /// Note: This returns a `Vec` but should guarantee that all hashes are unique.
    ///
    /// Consumer: Utility
    fn all_transaction_hashes(&self) -> Vec<TxHash>;

    /// Removes a single transaction corresponding to the given hash.
    ///
    /// Note: This removes the transaction as if it got discarded (_not_ mined).
    ///
    /// Returns the removed transaction if it was found in the pool.
    ///
    /// Consumer: Utility
    fn remove_transaction(&self, hash: TxHash) -> Option<Arc<ValidPoolTransaction>> {
        self.remove_transactions(vec![hash]).pop()
    }

    /// Removes all transactions corresponding to the given hashes.
    ///
    /// Note: This removes the transactions as if they got discarded (_not_ mined).
    ///
    /// Consumer: Utility
    fn remove_transactions(&self, hashes: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction>>;

    /// Removes all transactions corresponding to the given hashes.
    ///
    /// Also removes all _dependent_ transactions.
    ///
    /// Consumer: Utility
    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction>>;

    /// Removes all transactions from the given sender
    ///
    /// Consumer: Utility
    fn remove_transactions_by_sender(&self, sender: Address) -> Vec<Arc<ValidPoolTransaction>>;

    /// Prunes a single transaction from the pool.
    ///
    /// This is similar to [`Self::remove_transaction`] but treats the transaction as _mined_
    /// rather than discarded. The key difference is that pruning does **not** park descendant
    /// transactions: their nonce requirements are considered satisfied, so they remain in whatever
    /// sub-pool they currently occupy and can be included in the next block.
    ///
    /// In contrast, [`Self::remove_transaction`] treats the removal as a discard, which
    /// introduces a nonce gap and moves all descendant transactions to the queued (parked)
    /// sub-pool.
    ///
    /// Returns the pruned transaction if it existed in the pool.
    ///
    /// Consumer: Utility
    fn prune_transaction(&self, hash: TxHash) -> Option<Arc<ValidPoolTransaction>> {
        self.prune_transactions(vec![hash]).pop()
    }

    /// Prunes all transactions corresponding to the given hashes from the pool.
    ///
    /// This behaves like [`Self::prune_transaction`] but for multiple transactions at once.
    /// Each transaction is removed as if it was mined: descendant transactions are **not** parked
    /// and their nonce requirements are considered satisfied.
    ///
    /// This is useful for scenarios like Flashblocks where transactions are committed across
    /// multiple partial blocks without a canonical state update: previously committed transactions
    /// can be pruned so that the best-transactions iterator yields their descendants in the
    /// correct priority order.
    ///
    /// Consumer: Utility
    fn prune_transactions(&self, hashes: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction>>;

    /// Retains only those hashes that are unknown to the pool.
    ///
    /// In other words, removes all transactions from the given set that are currently present in
    /// the pool.
    ///
    /// Consumer: P2P
    fn retain_unknown<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData;

    /// Retains only those hashes that are known to the pool.
    ///
    /// In other words, removes all transactions from the given set that are not currently present
    /// in the pool.
    ///
    /// Consumer: P2P
    fn retain_contains<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData;

    /// Returns if the transaction for the given hash is already included in this pool.
    fn contains(&self, tx_hash: &TxHash) -> bool {
        self.get(tx_hash).is_some()
    }

    /// Returns the transaction for the given hash.
    fn get(&self, tx_hash: &TxHash) -> Option<Arc<ValidPoolTransaction>>;

    /// Returns all transaction objects for the given hashes.
    ///
    /// Caution: In case of blob transactions, this does not include the sidecar.
    fn get_all(&self, txs: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction>>;

    /// Notify the pool about transactions that are propagated to peers.
    ///
    /// Consumer: P2P
    fn on_propagated(&self, txs: PropagatedTransactions);

    /// Returns all transactions sent by a given user
    fn get_transactions_by_sender(&self, sender: Address) -> Vec<Arc<ValidPoolTransaction>>;

    /// Returns all pending transactions filtered by predicate
    fn get_pending_transactions_with_predicate(
        &self,
        predicate: impl FnMut(&ValidPoolTransaction) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction>>;

    /// Returns all pending transactions sent by a given user
    fn get_pending_transactions_by_sender(&self, sender: Address)
    -> Vec<Arc<ValidPoolTransaction>>;

    /// Returns all queued transactions sent by a given user
    fn get_queued_transactions_by_sender(&self, sender: Address) -> Vec<Arc<ValidPoolTransaction>>;

    /// Returns the highest transaction sent by a given user
    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction>>;

    /// Returns the transaction with the highest nonce that is executable given the on chain nonce.
    /// In other words the highest non nonce gapped transaction.
    ///
    /// Note: The next pending pooled transaction must have the on chain nonce.
    ///
    /// For example, for a given on chain nonce of `5`, the next transaction must have that nonce.
    /// If the pool contains txs `[5,6,7]` this returns tx `7`.
    /// If the pool contains txs `[6,7]` this returns `None` because the next valid nonce (5) is
    /// missing, which means txs `[6,7]` are nonce gapped.
    fn get_highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction>>;

    /// Returns a transaction sent by a given user and a nonce
    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction>>;

    /// Returns all transactions that where submitted with the given [`TransactionOrigin`]
    fn get_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction>>;

    /// Returns all pending transactions filtered by [`TransactionOrigin`]
    fn get_pending_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction>>;

    /// Returns all transactions that where submitted as [`TransactionOrigin::Local`]
    fn get_local_transactions(&self) -> Vec<Arc<ValidPoolTransaction>> {
        self.get_transactions_by_origin(TransactionOrigin::Local)
    }

    /// Returns all transactions that where submitted as [`TransactionOrigin::Private`]
    fn get_private_transactions(&self) -> Vec<Arc<ValidPoolTransaction>> {
        self.get_transactions_by_origin(TransactionOrigin::Private)
    }

    /// Returns all transactions that where submitted as [`TransactionOrigin::External`]
    fn get_external_transactions(&self) -> Vec<Arc<ValidPoolTransaction>> {
        self.get_transactions_by_origin(TransactionOrigin::External)
    }

    /// Returns all pending transactions that where submitted as [`TransactionOrigin::Local`]
    fn get_local_pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction>> {
        self.get_pending_transactions_by_origin(TransactionOrigin::Local)
    }

    /// Returns all pending transactions that where submitted as [`TransactionOrigin::Private`]
    fn get_private_pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction>> {
        self.get_pending_transactions_by_origin(TransactionOrigin::Private)
    }

    /// Returns all pending transactions that where submitted as [`TransactionOrigin::External`]
    fn get_external_pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction>> {
        self.get_pending_transactions_by_origin(TransactionOrigin::External)
    }

    /// Returns a set of all senders of transactions in the pool
    fn unique_senders(&self) -> AddressSet;

    /// Returns the [`BlobTransactionSidecarVariant`] for the given transaction hash if it exists in
    /// the blob store.
    fn get_blob(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<Arc<BlobTransactionSidecarVariant>>, BlobStoreError>;

    /// Returns all [`BlobTransactionSidecarVariant`] for the given transaction hashes if they
    /// exists in the blob store.
    ///
    /// This only returns the blobs that were found in the store.
    /// If there's no blob it will not be returned.
    fn get_all_blobs(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<(TxHash, Arc<BlobTransactionSidecarVariant>)>, BlobStoreError>;

    /// Returns the exact [`BlobTransactionSidecarVariant`] for the given transaction hashes in the
    /// order they were requested.
    ///
    /// Returns an error if any of the blobs are not found in the blob store.
    fn get_all_blobs_exact(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<Arc<BlobTransactionSidecarVariant>>, BlobStoreError>;

    /// Return the [`BlobAndProofV1`]s for a list of blob versioned hashes.
    fn get_blobs_for_versioned_hashes_v1(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV1>>, BlobStoreError>;

    /// Return the [`BlobAndProofV2`]s for a list of blob versioned hashes.
    /// Blobs and proofs are returned only if they are present for _all_ of the requested versioned
    /// hashes.
    fn get_blobs_for_versioned_hashes_v2(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Option<Vec<BlobAndProofV2>>, BlobStoreError>;

    /// Return the [`BlobAndProofV2`]s for a list of blob versioned hashes.
    ///
    /// The response is always the same length as the request. Missing or older-version blobs are
    /// returned as `None` elements.
    fn get_blobs_for_versioned_hashes_v3(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV2>>, BlobStoreError>;

    /// Return the [`BlobCellsAndProofsV1`]s for a list of blob versioned hashes and requested cell
    /// indices.
    ///
    /// The response is always the same length as the request. Missing or older-version blobs are
    /// returned as `None` elements.
    fn get_blobs_for_versioned_hashes_v4(
        &self,
        versioned_hashes: &[B256],
        indices_bitarray: B128,
    ) -> Result<Vec<Option<BlobCellsAndProofsV1>>, BlobStoreError>;

    /// Return whether each requested blob versioned hash is available.
    ///
    /// The response is always the same length and order as the request.
    fn has_blobs_for_versioned_hashes(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<bool>, BlobStoreError>;

    /// Returns the blob store used by the pool.
    fn blob_store(&self) -> Box<dyn BlobStore>;
}

/// Extension for [`TransactionPool`] trait that allows to set the current block info.
#[auto_impl::auto_impl(&, Arc)]
pub trait TransactionPoolExt: TransactionPool {
    /// Sets the current block info for the pool.
    fn set_block_info(&self, info: BlockInfo);

    /// Event listener for when the pool needs to be updated.
    ///
    /// Implementers need to update the pool accordingly:
    ///
    /// ## Fee changes
    ///
    /// The [`CanonicalStateUpdate`] includes the base and blob fee of the pending block, which
    /// affects the dynamic fee requirement of pending transactions in the pool.
    ///
    /// ## EIP-4844 Blob transactions
    ///
    /// Mined blob transactions need to be removed from the pool, but from the pool only. The blob
    /// sidecar must not be removed from the blob store. Only after a blob transaction is
    /// finalized, its sidecar is removed from the blob store. This ensures that in case of a reorg,
    /// the sidecar is still available.
    fn on_canonical_state_change(&self, update: CanonicalStateUpdate<'_>);

    /// Updates the accounts in the pool
    fn update_accounts(&self, accounts: Vec<ChangedAccount>);

    /// Deletes the blob sidecar for the given transaction from the blob store
    fn delete_blob(&self, tx: B256);

    /// Deletes multiple blob sidecars from the blob store
    fn delete_blobs(&self, txs: Vec<B256>);

    /// Maintenance function to cleanup blobs that are no longer needed.
    fn cleanup_blobs(&self);
}

/// Extension for [`TransactionPool`] that exposes the pool's underlying [`TransactionValidator`].
///
/// This is implemented by pools that validate transactions through a single validator before
/// insertion (e.g. [`Pool`](crate::Pool)). It lets consumers and wrapper pools reach the validator
/// directly, for example to validate a transaction without inserting it into the pool.
pub trait ValidatingPool: TransactionPool {
    /// The validator used to validate transactions before they are inserted into the pool.
    type Validator: TransactionValidator;

    /// Returns a reference to the pool's transaction validator.
    fn validator(&self) -> &Self::Validator;

    /// Validates the given transaction without inserting it into the pool.
    ///
    /// This is a convenience wrapper around [`TransactionValidator::validate_transaction`].
    fn validate(
        &self,
        origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
    ) -> impl Future<Output = TransactionValidationOutcome> + Send {
        self.validator().validate_transaction(origin, transaction)
    }
}

/// A Helper type that bundles all transactions in the pool.
#[derive(Debug, Clone)]
pub struct AllPoolTransactions {
    /// Transactions that are ready for inclusion in the next block.
    pub pending: Vec<Arc<ValidPoolTransaction>>,
    /// Transactions that are ready for inclusion in _future_ blocks, but are currently parked,
    /// because they depend on other transactions that are not yet included in the pool (nonce gap)
    /// or otherwise blocked.
    pub queued: Vec<Arc<ValidPoolTransaction>>,
}

// === impl AllPoolTransactions ===

impl AllPoolTransactions {
    /// Returns the combined number of all transactions.
    pub const fn count(&self) -> usize {
        self.pending.len() + self.queued.len()
    }

    /// Returns an iterator over all pending and queued transactions.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<ValidPoolTransaction>> + '_ {
        self.pending.iter().chain(self.queued.iter())
    }

    /// Returns an iterator over all pending [`Recovered`] transactions.
    pub fn pending_recovered(
        &self,
    ) -> impl Iterator<Item = Recovered<base_common_types_chain::BaseTxEnvelope>> + '_ {
        self.pending.iter().map(|tx| tx.to_consensus())
    }

    /// Returns an iterator over all queued [`Recovered`] transactions.
    pub fn queued_recovered(
        &self,
    ) -> impl Iterator<Item = Recovered<base_common_types_chain::BaseTxEnvelope>> + '_ {
        self.queued.iter().map(|tx| tx.to_consensus())
    }

    /// Returns an iterator over all transactions, both pending and queued.
    pub fn all(
        &self,
    ) -> impl Iterator<Item = Recovered<base_common_types_chain::BaseTxEnvelope>> + '_ {
        self.pending.iter().chain(self.queued.iter()).map(|tx| tx.to_consensus())
    }
}

impl Default for AllPoolTransactions {
    fn default() -> Self {
        Self { pending: Default::default(), queued: Default::default() }
    }
}

impl IntoIterator for AllPoolTransactions {
    type Item = Arc<ValidPoolTransaction>;
    type IntoIter = std::iter::Chain<
        std::vec::IntoIter<Arc<ValidPoolTransaction>>,
        std::vec::IntoIter<Arc<ValidPoolTransaction>>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.pending.into_iter().chain(self.queued)
    }
}

/// Represents transactions that were propagated over the network.
#[derive(Debug, Clone, Eq, PartialEq, Default)]
pub struct PropagatedTransactions(pub B256Map<Vec<PropagateKind>>);

impl PropagatedTransactions {
    /// Records a propagation of a transaction to a peer.
    pub fn record(&mut self, hash: TxHash, kind: PropagateKind) {
        self.0.entry(hash).or_default().push(kind);
    }

    /// Returns the number of distinct transactions that were propagated.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns true if no transactions were propagated.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the propagation info for a specific transaction.
    pub fn get(&self, hash: &TxHash) -> Option<&[PropagateKind]> {
        self.0.get(hash).map(Vec::as_slice)
    }
}

impl IntoIterator for PropagatedTransactions {
    type Item = (TxHash, Vec<PropagateKind>);
    type IntoIter = alloy_primitives::map::hash_map::IntoIter<TxHash, Vec<PropagateKind>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// Represents how a transaction was propagated over the network.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PropagateKind {
    /// The full transaction object was sent to the peer.
    ///
    /// This is equivalent to the `Transaction` message
    Full(PeerId),
    /// Only the Hash was propagated to the peer.
    Hash(PeerId),
}

// === impl PropagateKind ===

impl PropagateKind {
    /// Returns the peer the transaction was sent to
    pub const fn peer(&self) -> &PeerId {
        match self {
            Self::Full(peer) | Self::Hash(peer) => peer,
        }
    }

    /// Returns true if the transaction was sent as a full transaction
    pub const fn is_full(&self) -> bool {
        matches!(self, Self::Full(_))
    }

    /// Returns true if the transaction was sent as a hash
    pub const fn is_hash(&self) -> bool {
        matches!(self, Self::Hash(_))
    }
}

impl From<PropagateKind> for PeerId {
    fn from(value: PropagateKind) -> Self {
        match value {
            PropagateKind::Full(peer) | PropagateKind::Hash(peer) => peer,
        }
    }
}

/// This type represents a new blob sidecar that has been stored in the transaction pool's
/// blobstore; it includes the `TransactionHash` of the blob transaction along with the assoc.
/// sidecar (blobs, commitments, proofs)
#[derive(Debug, Clone)]
pub struct NewBlobSidecar {
    /// hash of the EIP-4844 transaction.
    pub tx_hash: TxHash,
    /// the blob transaction sidecar.
    pub sidecar: Arc<BlobTransactionSidecarVariant>,
}

/// Where the transaction originates from.
///
/// Depending on where the transaction was picked up, it affects how the transaction is handled
/// internally, e.g. limits for simultaneous transaction of one sender.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub enum TransactionOrigin {
    /// Transaction is coming from a local source.
    #[default]
    Local,
    /// Transaction has been received externally.
    ///
    /// This is usually considered an "untrusted" source, for example received from another in the
    /// network.
    External,
    /// Transaction is originated locally and is intended to remain private.
    ///
    /// This type of transaction should not be propagated to the network. It's meant for
    /// private usage within the local node only.
    Private,
}

// === impl TransactionOrigin ===

impl TransactionOrigin {
    /// Whether the transaction originates from a local source.
    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    /// Whether the transaction originates from an external source.
    pub const fn is_external(&self) -> bool {
        matches!(self, Self::External)
    }
    /// Whether the transaction originates from a private source.
    pub const fn is_private(&self) -> bool {
        matches!(self, Self::Private)
    }
}

/// Represents the kind of update to the canonical state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolUpdateKind {
    /// The update was due to a block commit.
    Commit,
    /// The update was due to a reorganization.
    Reorg,
}

/// Represents changes after a new canonical block or range of canonical blocks was added to the
/// chain.
///
/// It is expected that this is only used if the added blocks are canonical to the pool's last known
/// block hash. In other words, the first added block of the range must be the child of the last
/// known block hash.
///
/// This is used to update the pool state accordingly.
#[derive(Clone, Debug)]
pub struct CanonicalStateUpdate<'a> {
    /// Hash of the tip block.
    pub new_tip: &'a SealedBlock,
    /// EIP-1559 Base fee of the _next_ (pending) block
    ///
    /// The base fee of a block depends on the utilization of the last block and its base fee.
    pub pending_block_base_fee: u64,
    /// EIP-4844 blob fee of the _next_ (pending) block
    ///
    /// Only after Cancun
    pub pending_block_blob_fee: Option<u128>,
    /// A set of changed accounts across a range of blocks.
    pub changed_accounts: Vec<ChangedAccount>,
    /// All mined transactions in the block range.
    pub mined_transactions: Vec<B256>,
    /// The kind of update to the canonical state.
    pub update_kind: PoolUpdateKind,
}

impl CanonicalStateUpdate<'_> {
    /// Returns the number of the tip block.
    pub fn number(&self) -> u64 {
        self.new_tip.number()
    }

    /// Returns the hash of the tip block.
    pub fn hash(&self) -> B256 {
        self.new_tip.hash()
    }

    /// Timestamp of the latest chain update
    pub fn timestamp(&self) -> u64 {
        self.new_tip.timestamp()
    }

    /// Returns the block info for the tip block.
    pub fn block_info(&self) -> BlockInfo {
        BlockInfo {
            block_gas_limit: self.new_tip.gas_limit(),
            last_seen_block_hash: self.hash(),
            last_seen_block_number: self.number(),
            pending_basefee: self.pending_block_base_fee,
            pending_blob_fee: self.pending_block_blob_fee,
        }
    }
}

impl fmt::Display for CanonicalStateUpdate<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CanonicalStateUpdate")
            .field("hash", &self.hash())
            .field("number", &self.number())
            .field("pending_block_base_fee", &self.pending_block_base_fee)
            .field("pending_block_blob_fee", &self.pending_block_blob_fee)
            .field("changed_accounts", &self.changed_accounts.len())
            .field("mined_transactions", &self.mined_transactions.len())
            .finish()
    }
}

/// Alias to restrict the [`BestTransactions`] items to the pool's transaction type.
pub type BestTransactionsFor = Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction>>>;

/// An `Iterator` that only returns transactions that are ready to be executed.
///
/// This makes no assumptions about the order of the transactions, but expects that _all_
/// transactions are valid (no nonce gaps.) for the tracked state of the pool.
///
/// Note: this iterator will always return the best transaction that it currently knows.
/// There is no guarantee transactions will be returned sequentially in decreasing
/// priority order.
pub trait BestTransactions: Iterator + Send {
    /// Mark the transaction as invalid.
    ///
    /// Implementers must ensure all subsequent transaction _don't_ depend on this transaction.
    /// In other words, this must remove the given transaction _and_ drain all transaction that
    /// depend on it.
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError);

    /// An iterator may be able to receive additional pending transactions that weren't present it
    /// the pool when it was created.
    ///
    /// This ensures that iterator will return the best transaction that it currently knows and not
    /// listen to pool updates.
    fn no_updates(&mut self);

    /// Allows newly received transactions to be yielded even if their priority is higher than a
    /// transaction that was already yielded.
    ///
    /// This is useful for long-lived consumers that prefer seeing every update over preserving
    /// decreasing priority order. The default implementation leaves the iterator's ordering
    /// behavior unchanged. Implementations must still preserve transaction dependency ordering.
    fn allow_updates_out_of_order(&mut self) {}

    /// Convenience function for [`Self::no_updates`] that returns the iterator again.
    fn without_updates(mut self) -> Self
    where
        Self: Sized,
    {
        self.no_updates();
        self
    }

    /// Skip all blob transactions.
    ///
    /// There's only limited blob space available in a block, once exhausted, EIP-4844 transactions
    /// can no longer be included.
    ///
    /// If called then the iterator will no longer yield blob transactions.
    ///
    /// Note: this will also exclude any transactions that depend on blob transactions.
    fn skip_blobs(&mut self) {
        self.set_skip_blobs(true);
    }

    /// Controls whether the iterator skips blob transactions or not.
    ///
    /// If set to true, no blob transactions will be returned.
    fn set_skip_blobs(&mut self, skip_blobs: bool);

    /// Convenience function for [`Self::skip_blobs`] that returns the iterator again.
    fn without_blobs(mut self) -> Self
    where
        Self: Sized,
    {
        self.skip_blobs();
        self
    }

    /// Creates an iterator which uses a closure to determine whether a transaction should be
    /// returned by the iterator.
    ///
    /// All items the closure returns false for are marked as invalid via [`Self::mark_invalid`] and
    /// descendant transactions will be skipped.
    fn filter_transactions<P>(self, predicate: P) -> BestTransactionFilter<Self, P>
    where
        P: FnMut(&Self::Item) -> bool,
        Self: Sized,
    {
        BestTransactionFilter::new(self, predicate)
    }
}

impl<T> BestTransactions for Box<T>
where
    T: BestTransactions + ?Sized,
{
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError) {
        (**self).mark_invalid(transaction, kind)
    }

    fn no_updates(&mut self) {
        (**self).no_updates();
    }

    fn allow_updates_out_of_order(&mut self) {
        (**self).allow_updates_out_of_order();
    }

    fn skip_blobs(&mut self) {
        (**self).skip_blobs();
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        (**self).set_skip_blobs(skip_blobs);
    }
}

/// A no-op implementation that yields no transactions.
impl<T> BestTransactions for std::iter::Empty<T> {
    fn mark_invalid(&mut self, _tx: &T, _kind: InvalidPoolTransactionError) {}

    fn no_updates(&mut self) {}

    fn skip_blobs(&mut self) {}

    fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
}

/// A filter that allows to check if a transaction satisfies a set of conditions
pub trait TransactionFilter {
    /// The type of the transaction to check.
    type Transaction;

    /// Returns true if the transaction satisfies the conditions.
    fn is_valid(&self, transaction: &Self::Transaction) -> bool;
}

/// A no-op implementation of [`TransactionFilter`] which
/// marks all transactions as valid.
#[derive(Debug, Clone)]
pub struct NoopTransactionFilter<T>(std::marker::PhantomData<T>);

// We can't derive Default because this forces T to be
// Default as well, which isn't necessary.
impl<T> Default for NoopTransactionFilter<T> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<T> TransactionFilter for NoopTransactionFilter<T> {
    type Transaction = T;

    fn is_valid(&self, _transaction: &Self::Transaction) -> bool {
        true
    }
}

/// A Helper type that bundles the best transactions attributes together.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct BestTransactionsAttributes {
    /// The base fee attribute for best transactions.
    pub basefee: u64,
    /// The blob fee attribute for best transactions.
    pub blob_fee: Option<u64>,
}

// === impl BestTransactionsAttributes ===

impl BestTransactionsAttributes {
    /// Creates a new `BestTransactionsAttributes` with the given basefee and blob fee.
    pub const fn new(basefee: u64, blob_fee: Option<u64>) -> Self {
        Self { basefee, blob_fee }
    }

    /// Creates a new `BestTransactionsAttributes` with the given basefee.
    pub const fn base_fee(basefee: u64) -> Self {
        Self::new(basefee, None)
    }

    /// Sets the given blob fee.
    pub const fn with_blob_fee(mut self, blob_fee: u64) -> Self {
        self.blob_fee = Some(blob_fee);
        self
    }
}

/// Represents the blob sidecar of the [`BasePooledTransaction`].
///
/// EIP-4844 blob transactions require additional data (blobs, commitments, proofs)
/// for validation that is not included in the consensus format. This enum tracks
/// the sidecar state throughout the transaction's lifecycle in the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EthBlobTransactionSidecar {
    /// This transaction does not have a blob sidecar
    /// (applies to all non-EIP-4844 transaction types)
    None,
    /// This transaction has a blob sidecar (EIP-4844) but it is missing.
    ///
    /// This can happen when:
    /// - The sidecar was extracted after the transaction was added to the pool
    /// - The transaction was re-injected after a reorg without its sidecar
    /// - The transaction was recovered from the consensus format (e.g., from a block)
    Missing,
    /// The EIP-4844 transaction was received from the network with its complete sidecar.
    ///
    /// This sidecar contains:
    /// - The actual blob data (large data per blob)
    /// - KZG commitments for each blob
    /// - KZG proofs for validation
    ///
    /// The sidecar is required for validating the transaction but is not included
    /// in blocks (only the blob hashes are included in the consensus format).
    Present(PooledBlobSidecar),
}

impl EthBlobTransactionSidecar {
    /// Returns the blob sidecar if it is present
    pub const fn maybe_sidecar(&self) -> Option<&BlobTransactionSidecarVariant> {
        match self {
            Self::Present(sidecar) => Some(sidecar.sidecar()),
            _ => None,
        }
    }
}

/// Represents the current status of the pool.
#[derive(Debug, Clone, Copy, Default)]
pub struct PoolSize {
    /// Number of transactions in the _pending_ sub-pool.
    pub pending: usize,
    /// Reported size of transactions in the _pending_ sub-pool.
    pub pending_size: usize,
    /// Number of transactions in the _blob_ pool.
    pub blob: usize,
    /// Reported size of transactions in the _blob_ pool.
    pub blob_size: usize,
    /// Number of transactions in the _basefee_ pool.
    pub basefee: usize,
    /// Reported size of transactions in the _basefee_ sub-pool.
    pub basefee_size: usize,
    /// Number of transactions in the _queued_ sub-pool.
    pub queued: usize,
    /// Reported size of transactions in the _queued_ sub-pool.
    pub queued_size: usize,
    /// Number of all transactions of all sub-pools
    ///
    /// Note: this is the sum of ```pending + basefee + queued + blob```
    pub total: usize,
}

// === impl PoolSize ===

impl PoolSize {
    /// Asserts that the invariants of the pool size are met.
    #[cfg(test)]
    pub fn assert_invariants(&self) {
        assert_eq!(self.total, self.pending + self.basefee + self.queued + self.blob);
    }
}

/// Represents the current status of the pool.
#[derive(Default, Debug, Clone, Copy, Eq, PartialEq)]
pub struct BlockInfo {
    /// Hash for the currently tracked block.
    pub last_seen_block_hash: B256,
    /// Currently tracked block.
    pub last_seen_block_number: u64,
    /// Current block gas limit for the latest block.
    pub block_gas_limit: u64,
    /// Currently enforced base fee: the threshold for the basefee sub-pool.
    ///
    /// Note: this is the derived base fee of the _next_ block that builds on the block the pool is
    /// currently tracking.
    pub pending_basefee: u64,
    /// Currently enforced blob fee: the threshold for eip-4844 blob transactions.
    ///
    /// Note: this is the derived blob fee of the _next_ block that builds on the block the pool is
    /// currently tracking
    pub pending_blob_fee: Option<u128>,
}

/// The limit to enforce for [`TransactionPool::get_pooled_transaction_elements`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum GetPooledTransactionLimit {
    /// No limit, return all transactions.
    None,
    /// Enforce a size limit on the returned transactions, for example 2MB
    ResponseSizeSoftLimit(usize),
}

impl GetPooledTransactionLimit {
    /// Returns true if the given size exceeds the limit.
    #[inline]
    pub const fn exceeds(&self, size: usize) -> bool {
        match self {
            Self::None => false,
            Self::ResponseSizeSoftLimit(limit) => size > *limit,
        }
    }
}

/// A Stream that yields full transactions the subpool
#[must_use = "streams do nothing unless polled"]
#[derive(Debug)]
pub struct NewSubpoolTransactionStream {
    st: Receiver<NewTransactionEvent>,
    subpool: SubPool,
}

// === impl NewSubpoolTransactionStream ===

impl NewSubpoolTransactionStream {
    /// Create a new stream that yields full transactions from the subpool
    pub const fn new(st: Receiver<NewTransactionEvent>, subpool: SubPool) -> Self {
        Self { st, subpool }
    }

    /// Tries to receive the next value for this stream.
    pub fn try_recv(
        &mut self,
    ) -> Result<NewTransactionEvent, tokio::sync::mpsc::error::TryRecvError> {
        loop {
            let event = self.st.try_recv()?;
            if event.subpool == self.subpool {
                return Ok(event);
            }
        }
    }
}

impl Stream for NewSubpoolTransactionStream {
    type Item = NewTransactionEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match ready!(self.st.poll_recv(cx)) {
                Some(event) => {
                    if event.subpool == self.subpool {
                        return Poll::Ready(Some(event));
                    }
                }
                None => return Poll::Ready(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_pool_size_invariants() {
        let pool_size = PoolSize {
            pending: 10,
            pending_size: 1000,
            blob: 5,
            blob_size: 500,
            basefee: 8,
            basefee_size: 800,
            queued: 7,
            queued_size: 700,
            total: 10 + 5 + 8 + 7, // Correct total
        };

        // Call the assert_invariants method to check if the invariants are correct
        pool_size.assert_invariants();
    }

    #[test]
    #[should_panic]
    fn test_pool_size_invariants_fail() {
        let pool_size = PoolSize {
            pending: 10,
            pending_size: 1000,
            blob: 5,
            blob_size: 500,
            basefee: 8,
            basefee_size: 800,
            queued: 7,
            queued_size: 700,
            total: 10 + 5 + 8, // Incorrect total
        };

        // Call the assert_invariants method, which should panic
        pool_size.assert_invariants();
    }

    #[test]
    fn test_pooled_transaction_limit() {
        // No limit should never exceed
        let limit_none = GetPooledTransactionLimit::None;
        // Any size should return false
        assert!(!limit_none.exceeds(1000));

        // Size limit of 2MB (2 * 1024 * 1024 bytes)
        let size_limit_2mb = GetPooledTransactionLimit::ResponseSizeSoftLimit(2 * 1024 * 1024);

        // Test with size below the limit
        // 1MB is below 2MB, should return false
        assert!(!size_limit_2mb.exceeds(1024 * 1024));

        // Test with size exactly at the limit
        // 2MB equals the limit, should return false
        assert!(!size_limit_2mb.exceeds(2 * 1024 * 1024));

        // Test with size exceeding the limit
        // 3MB is above the 2MB limit, should return true
        assert!(size_limit_2mb.exceeds(3 * 1024 * 1024));
    }
}
