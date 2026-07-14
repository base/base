//! Base transaction-pool wrapper that combines the protocol pool with a 2D nonce sidecar.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use alloy_eips::{
    eip4844::{BlobAndProofV1, BlobAndProofV2, BlobCellsAndProofsV1},
    eip7594::BlobTransactionSidecarVariant,
};
use alloy_primitives::{Address, B128, B256, TxHash, map::AddressSet};
use futures::StreamExt;
use parking_lot::RwLock;
use reth_eth_wire_types::HandleMempoolData;
use reth_execution_types::ChangedAccount;
use reth_primitives_traits::Recovered;
use reth_transaction_pool::{
    AddedTransactionOutcome, AllPoolTransactions, AllTransactionsEvents, BestTransactions,
    BestTransactionsAttributes, BlobStore, BlobStoreError, BlockInfo, FullTransactionEvent,
    GetPooledTransactionLimit, NewBlobSidecar, NewTransactionEvent, Pool, PoolResult, PoolSize,
    PoolTransaction, PropagatedTransactions, SubPool, TransactionEvents, TransactionListenerKind,
    TransactionOrigin, TransactionPool, TransactionPoolExt, TransactionValidationOutcome,
    TransactionValidationTaskExecutor, TransactionValidator, ValidPoolTransaction,
    pool::{AddedTransactionState, TransactionEvent},
};
use tokio::{spawn, sync::mpsc};

use crate::{
    BasePooledTx, BaseTransactionValidator,
    best::MergeBestTransactions,
    two_d_nonce_pool::{InsertOutcome, TwoDNoncePool},
};

const SIDE_CAR_EVENT_CHANNEL_SIZE: usize = 1024;

/// Wrapper around reth's transaction pool that adds a 2D nonce sidecar for EIP-8130 channels.
pub struct BaseTransactionPool<
    Client,
    S,
    Evm,
    T = crate::BasePooledTransaction,
    O = crate::BaseOrdering<T>,
> where
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    protocol_pool:
        Pool<TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>>, O, S>,
    ordering: O,
    nonce_pool: Arc<RwLock<TwoDNoncePool<T>>>,
    eip8130_replays: Arc<RwLock<HashMap<B256, TxHash>>>,
    listeners: Arc<RwLock<SidecarListeners<T>>>,
}

impl<Client, S, Evm, T, O> fmt::Debug for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BaseTransactionPool").finish_non_exhaustive()
    }
}

impl<Client, S, Evm, T, O> Clone for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    fn clone(&self) -> Self {
        Self {
            protocol_pool: self.protocol_pool.clone(),
            ordering: self.ordering.clone(),
            nonce_pool: Arc::clone(&self.nonce_pool),
            eip8130_replays: Arc::clone(&self.eip8130_replays),
            listeners: Arc::clone(&self.listeners),
        }
    }
}

impl<Client, S, Evm, T, O> Unpin for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
}

impl<Client, S, Evm, T, O> BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    /// Creates a new wrapper around the reth protocol pool.
    pub fn new(
        protocol_pool: Pool<
            TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>>,
            O,
            S,
        >,
        ordering: O,
    ) -> Self {
        let price_bump_config = protocol_pool.config().price_bumps;
        Self {
            protocol_pool,
            ordering,
            nonce_pool: Arc::new(RwLock::new(TwoDNoncePool::new(price_bump_config))),
            eip8130_replays: Arc::new(RwLock::new(HashMap::new())),
            listeners: Arc::new(RwLock::new(SidecarListeners::default())),
        }
    }

    /// Returns the wrapped reth pool.
    pub const fn protocol_pool(
        &self,
    ) -> &Pool<TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>>, O, S>
    {
        &self.protocol_pool
    }

    /// Returns the validator backing the wrapped reth pool.
    pub fn validator(
        &self,
    ) -> &TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>> {
        self.protocol_pool.validator()
    }

    fn is_sidecar_transaction(&self, transaction: &T) -> bool {
        transaction.eip8130_nonce_channel_key().is_some()
    }

    /// Best-effort replay-id dedup lookup for the mempool admission path.
    ///
    /// This check and the subsequent [`Self::track_eip8130_replay_id`] insert are
    /// deliberately **not** atomic: the index lock is released between them (and
    /// between this check and the actual pool insert). Two concurrent admissions
    /// of the same `replay_id` can therefore both pass and enter the
    /// pool. That is acceptable because the index is only a mempool optimization,
    /// not a consensus control: identical transactions collapse by tx-hash in the
    /// underlying pool, and any surviving nonce-free duplicate is rejected at
    /// execution by the enshrined replay buffer. Holding the index lock across the
    /// validate+insert would also serialize admission and reintroduce cross-lock
    /// nesting, so the looser guarantee is intentional. A stale entry whose target
    /// is no longer pooled is opportunistically evicted here.
    fn eip8130_replay_already_seen(&self, transaction: &T) -> Option<TxHash> {
        // `replay_id` already commits to the resolved sender, so it alone keys the
        // index (matching the enshrined replay buffer, which keys by `replay_id`).
        let replay_id = transaction.eip8130_replay_id()?;
        let hash = self.eip8130_replays.read().get(&replay_id).copied()?;
        // Only nonce-free transactions have replay IDs, and those are only ever
        // admitted to the protocol pool; channelized transactions live in
        // `nonce_pool` and never carry a replay ID. Guard that routing invariant
        // so the protocol-pool-only liveness check below stays sound if routing
        // or replay-id derivation ever evolve independently.
        debug_assert!(
            !self.nonce_pool.read().contains(&hash),
            "eip8130 replay index points at a sidecar-pool transaction",
        );
        if self.protocol_pool.get(&hash).is_some() {
            return Some(hash);
        }
        self.eip8130_replays.write().remove(&replay_id);
        None
    }

    fn track_eip8130_replay_id(&self, replay_id: B256, hash: TxHash) {
        {
            let mut index = self.eip8130_replays.write();
            index.insert(replay_id, hash);
        }
        self.reconcile_eip8130_replays_if_needed();
    }

    fn reconcile_eip8130_replays_if_needed(&self) {
        let pool_size = self.pool_size().total;
        // Fast path: bail while within bound, holding only the index read lock.
        if self.eip8130_replays.read().len() <= pool_size {
            return;
        }
        // Snapshot and rebuild the index from the live pool *without* holding the
        // `eip8130_replays` lock. `pooled_transactions()` takes `nonce_pool.read()`,
        // so acquiring the index write lock first (as the naive `write(); rebuild()`
        // would) establishes an `eip8130_replays -> nonce_pool` lock order. Building
        // the replacement outside the lock keeps the two locks strictly disjoint,
        // avoiding a lock-order inversion with any path that touches the index after
        // the nonce pool. Entries added between this snapshot and the write below are
        // best-effort only (the index is a dedup optimization, not consensus state).
        let mut rebuilt = HashMap::new();
        for transaction in self.pooled_transactions() {
            if let Some(replay_id) = transaction.transaction.eip8130_replay_id() {
                rebuilt.insert(replay_id, *transaction.hash());
            }
        }
        let mut index = self.eip8130_replays.write();
        // Re-check under the write lock: only overwrite while still oversized.
        if index.len() > pool_size {
            *index = rebuilt;
        }
    }

    fn untrack_eip8130_replays(&self, transactions: &[Arc<ValidPoolTransaction<T>>]) {
        let mut index = self.eip8130_replays.write();
        for transaction in transactions {
            if let Some(replay_id) = transaction.transaction.eip8130_replay_id() {
                index.remove(&replay_id);
            }
        }
    }

    fn untrack_eip8130_hashes(&self, hashes: &[TxHash]) {
        let hashes = hashes.iter().collect::<HashSet<_>>();
        let mut index = self.eip8130_replays.write();
        index.retain(|_, indexed_hash| !hashes.contains(indexed_hash));
    }

    fn partition_hashes_by_pool(&self, hashes: Vec<TxHash>) -> (Vec<TxHash>, Vec<TxHash>) {
        let nonce_pool = self.nonce_pool.read();
        let mut protocol_hashes = Vec::with_capacity(hashes.len());
        let mut sidecar_hashes = Vec::new();

        for hash in hashes {
            if nonce_pool.contains(&hash) {
                sidecar_hashes.push(hash);
            } else {
                protocol_hashes.push(hash);
            }
        }

        (protocol_hashes, sidecar_hashes)
    }

    async fn add_sidecar_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: T,
    ) -> PoolResult<AddedTransactionOutcome> {
        let validated = self.validator().validate_transaction(origin, transaction).await;
        self.add_validated_sidecar_transaction(validated, origin)
    }

    fn add_validated_sidecar_transaction(
        &self,
        validated: TransactionValidationOutcome<T>,
        origin: TransactionOrigin,
    ) -> PoolResult<AddedTransactionOutcome> {
        match validated {
            TransactionValidationOutcome::Valid {
                transaction,
                propagate,
                authorities,
                state_nonce,
                ..
            } => {
                // Keep the sidecar lock order consistent everywhere: nonce_pool before listeners.
                let mut nonce_pool = self.nonce_pool.write();
                let mut listeners = self.listeners.write();
                let validated = self.validated_pool_transaction(
                    transaction,
                    origin,
                    propagate,
                    authorities,
                    &mut nonce_pool,
                );
                let outcome = nonce_pool.insert_validated(validated, state_nonce)?;
                listeners.on_inserted(&nonce_pool, &outcome);
                Ok(outcome.outcome)
            }
            TransactionValidationOutcome::Invalid(transaction, error) => {
                Err(reth_transaction_pool::error::PoolError::new(
                    *transaction.hash(),
                    reth_transaction_pool::error::PoolErrorKind::InvalidTransaction(error),
                ))
            }
            TransactionValidationOutcome::Error(hash, error) => {
                Err(reth_transaction_pool::error::PoolError::other(hash, error.to_string()))
            }
        }
    }

    fn validated_pool_transaction(
        &self,
        transaction: reth_transaction_pool::validate::ValidTransaction<T>,
        origin: TransactionOrigin,
        propagate: bool,
        authorities: Option<Vec<Address>>,
        nonce_pool: &mut TwoDNoncePool<T>,
    ) -> ValidPoolTransaction<T> {
        let transaction = transaction.into_transaction();
        let sender_id = nonce_pool
            .transactions_by_sender(transaction.sender())
            .first()
            .map(|transaction| transaction.sender_id())
            .unwrap_or_else(|| nonce_pool.sender_id_or_create(transaction.sender()));
        let authority_ids = authorities.map(|authorities| {
            authorities
                .into_iter()
                .map(|authority| nonce_pool.sender_id_or_create(authority))
                .collect()
        });

        ValidPoolTransaction {
            transaction_id: reth_transaction_pool::identifier::TransactionId::new(
                sender_id,
                transaction.nonce(),
            ),
            transaction,
            propagate,
            timestamp: std::time::Instant::now(),
            origin,
            authority_ids,
        }
    }

    fn merged_pending_listener(&self, kind: TransactionListenerKind) -> mpsc::Receiver<TxHash> {
        let protocol = self.protocol_pool.pending_transactions_listener_for(kind);
        let sidecar = self.listeners.write().subscribe_pending(kind);
        merge_receivers(protocol, sidecar)
    }

    fn merged_new_transactions_listener(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<NewTransactionEvent<T>> {
        let protocol = self.protocol_pool.new_transactions_listener_for(kind);
        let sidecar = self.listeners.write().subscribe_new_transactions(kind);
        merge_receivers(protocol, sidecar)
    }

    fn merged_all_transactions_listener(&self) -> AllTransactionsEvents<T> {
        let mut protocol = self.protocol_pool.all_transactions_event_listener();
        let mut sidecar = self.listeners.write().subscribe_all();
        let (tx, rx) = mpsc::channel(SIDE_CAR_EVENT_CHANNEL_SIZE);
        spawn(async move {
            let mut protocol_open = true;
            let mut sidecar_open = true;
            while protocol_open || sidecar_open {
                tokio::select! {
                    event = protocol.next(), if protocol_open => match event {
                        Some(event) => {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        None => protocol_open = false,
                    },
                    event = sidecar.next(), if sidecar_open => match event {
                        Some(event) => {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        None => sidecar_open = false,
                    }
                }
            }
        });
        AllTransactionsEvents::new(rx)
    }
}

impl<Client, S, Evm, T, O> TransactionPool for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    type Transaction = T;

    fn pool_size(&self) -> PoolSize {
        let mut size = self.protocol_pool.pool_size();
        let nonce_pool = self.nonce_pool.read();
        let (pending, queued) = nonce_pool.pending_and_queued_txn_count();
        let pending_size: usize =
            nonce_pool.pending_transactions().iter().map(|tx| tx.encoded_length()).sum();
        let queued_size: usize =
            nonce_pool.queued_transactions().iter().map(|tx| tx.encoded_length()).sum();
        size.pending += pending;
        size.pending_size += pending_size;
        size.queued += queued;
        size.queued_size += queued_size;
        size.total += pending + queued;
        size
    }

    fn block_info(&self) -> BlockInfo {
        self.protocol_pool.block_info()
    }

    async fn add_transaction_and_subscribe(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<TransactionEvents> {
        if self.eip8130_replay_already_seen(&transaction).is_some() {
            // TODO: Replace the indexed transaction when the new priority fee
            // satisfies the pool's configured price bump.
            return Err(reth_transaction_pool::error::PoolError::new(
                *transaction.hash(),
                reth_transaction_pool::error::PoolErrorKind::AlreadyImported,
            ));
        }
        if !self.is_sidecar_transaction(&transaction) {
            let replay_id = transaction.eip8130_replay_id();
            let hash = *transaction.hash();
            let events =
                self.protocol_pool.add_transaction_and_subscribe(origin, transaction).await?;
            if let Some(replay_id) = replay_id {
                self.track_eip8130_replay_id(replay_id, hash);
            }
            return Ok(events);
        }

        let hash = *transaction.hash();
        let (events, listener) = self.listeners.write().subscribe_hash(hash);
        if let Err(error) = self.add_sidecar_transaction(origin, transaction).await {
            self.listeners.write().unsubscribe_hash_listener(&hash, &listener);
            return Err(error);
        }
        Ok(events)
    }

    async fn add_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<AddedTransactionOutcome> {
        if self.eip8130_replay_already_seen(&transaction).is_some() {
            // TODO: Replace the indexed transaction when the new priority fee
            // satisfies the pool's configured price bump.
            return Err(reth_transaction_pool::error::PoolError::new(
                *transaction.hash(),
                reth_transaction_pool::error::PoolErrorKind::AlreadyImported,
            ));
        }
        if self.is_sidecar_transaction(&transaction) {
            self.add_sidecar_transaction(origin, transaction).await
        } else {
            let replay_id = transaction.eip8130_replay_id();
            let hash = *transaction.hash();
            let outcome = self.protocol_pool.add_transaction(origin, transaction).await?;
            if let Some(replay_id) = replay_id {
                self.track_eip8130_replay_id(replay_id, hash);
            }
            Ok(outcome)
        }
    }

    async fn add_transactions(
        &self,
        origin: TransactionOrigin,
        transactions: Vec<Self::Transaction>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        let mut results = Vec::with_capacity(transactions.len());
        for transaction in transactions {
            results.push(self.add_transaction(origin, transaction).await);
        }
        results
    }

    async fn add_transactions_with_origins(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        let mut results = Vec::with_capacity(transactions.len());
        for (origin, transaction) in transactions {
            results.push(self.add_transaction(origin, transaction).await);
        }
        results
    }

    fn transaction_event_listener(&self, tx_hash: TxHash) -> Option<TransactionEvents> {
        self.protocol_pool.transaction_event_listener(tx_hash).or_else(|| {
            self.nonce_pool
                .read()
                .contains(&tx_hash)
                .then(|| self.listeners.write().subscribe_hash(tx_hash).0)
        })
    }

    fn all_transactions_event_listener(&self) -> AllTransactionsEvents<Self::Transaction> {
        self.merged_all_transactions_listener()
    }

    fn pending_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<TxHash> {
        self.merged_pending_listener(kind)
    }

    fn blob_transaction_sidecars_listener(&self) -> mpsc::Receiver<NewBlobSidecar> {
        self.protocol_pool.blob_transaction_sidecars_listener()
    }

    fn new_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<NewTransactionEvent<Self::Transaction>> {
        self.merged_new_transactions_listener(kind)
    }

    fn pooled_transaction_hashes(&self) -> Vec<TxHash> {
        let mut hashes = self.protocol_pool.pooled_transaction_hashes();
        hashes.extend(
            self.nonce_pool
                .read()
                .all_transactions()
                .into_iter()
                .filter(|transaction| transaction.propagate)
                .map(|transaction| *transaction.hash()),
        );
        hashes
    }

    fn pooled_transaction_hashes_max(&self, max: usize) -> Vec<TxHash> {
        let mut hashes = self.protocol_pool.pooled_transaction_hashes_max(max);
        if hashes.len() >= max {
            return hashes;
        }

        let nonce_pool = self.nonce_pool.read();
        for transaction in nonce_pool.all_transactions() {
            if transaction.propagate {
                hashes.push(*transaction.hash());
                if hashes.len() >= max {
                    break;
                }
            }
        }
        hashes
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pooled_transactions();
        transactions.extend(
            self.nonce_pool
                .read()
                .all_transactions()
                .into_iter()
                .filter(|transaction| transaction.propagate),
        );
        transactions
    }

    fn pooled_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pooled_transactions_max(max);
        if transactions.len() >= max {
            return transactions;
        }

        let nonce_pool = self.nonce_pool.read();
        for transaction in nonce_pool.all_transactions() {
            if transaction.propagate {
                transactions.push(transaction);
                if transactions.len() >= max {
                    break;
                }
            }
        }
        transactions
    }

    fn get_pooled_transaction_elements(
        &self,
        tx_hashes: Vec<TxHash>,
        limit: GetPooledTransactionLimit,
    ) -> Vec<<Self::Transaction as PoolTransaction>::Pooled> {
        let mut pooled = Vec::new();
        self.append_pooled_transaction_elements(&tx_hashes, limit, &mut pooled);
        pooled
    }

    fn append_pooled_transaction_elements(
        &self,
        tx_hashes: &[TxHash],
        limit: GetPooledTransactionLimit,
        out: &mut Vec<<Self::Transaction as PoolTransaction>::Pooled>,
    ) {
        let mut current_size = 0;
        for hash in tx_hashes {
            if let Some(transaction) = self.protocol_pool.get(hash) {
                let Some((pooled, encoded_length)) = pooled_element(&transaction) else {
                    continue;
                };
                current_size += encoded_length;
                if limit.exceeds(current_size) {
                    break;
                }
                out.push(pooled);
                continue;
            }

            let Some(transaction) = self.nonce_pool.read().get(hash) else {
                continue;
            };
            let Some((pooled, encoded_length)) = pooled_element(&transaction) else {
                continue;
            };
            current_size += encoded_length;
            if limit.exceeds(current_size) {
                break;
            }
            out.push(pooled);
        }
    }

    fn get_pooled_transaction_element(
        &self,
        tx_hash: TxHash,
    ) -> Option<Recovered<<Self::Transaction as PoolTransaction>::Pooled>> {
        self.protocol_pool.get_pooled_transaction_element(tx_hash).or_else(|| {
            self.nonce_pool
                .read()
                .get(&tx_hash)
                .and_then(|transaction| transaction.transaction.clone().try_into_pooled().ok())
        })
    }

    fn best_transactions(
        &self,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        let block_info = self.protocol_pool.block_info();
        let best_transactions_attributes = BestTransactionsAttributes::new(
            block_info.pending_basefee,
            block_info.pending_blob_fee.map(|fee| u64::try_from(fee).unwrap_or(u64::MAX)),
        );
        let base_fee = best_transactions_attributes.basefee;
        Box::new(MergeBestTransactions::new(
            self.protocol_pool.best_transactions_with_attributes(best_transactions_attributes),
            Box::new(self.nonce_pool.read().best_transactions(self.ordering.clone(), base_fee)),
            self.ordering.clone(),
            base_fee,
        ))
    }

    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        let base_fee = best_transactions_attributes.basefee;
        Box::new(MergeBestTransactions::new(
            self.protocol_pool.best_transactions_with_attributes(best_transactions_attributes),
            Box::new(self.nonce_pool.read().best_transactions(self.ordering.clone(), base_fee)),
            self.ordering.clone(),
            base_fee,
        ))
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pending_transactions();
        transactions.extend(self.nonce_pool.read().pending_transactions());
        transactions
    }

    fn get_pending_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        // Channelized nonce sequences live in a separate namespace from account nonces, so this
        // sender+nonce lookup intentionally remains protocol-only.
        self.protocol_pool.get_pending_transaction_by_sender_and_nonce(sender, nonce)
    }

    fn pending_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.pending_transactions_max(max);
        if transactions.len() >= max {
            return transactions;
        }

        let remaining = max - transactions.len();
        transactions
            .extend(self.nonce_pool.read().pending_transactions().into_iter().take(remaining));
        transactions
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.queued_transactions();
        transactions.extend(self.nonce_pool.read().queued_transactions());
        transactions
    }

    fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        let (pending, queued) = self.protocol_pool.pending_and_queued_txn_count();
        let (sidecar_pending, sidecar_queued) =
            self.nonce_pool.read().pending_and_queued_txn_count();
        (pending + sidecar_pending, queued + sidecar_queued)
    }

    fn all_transactions(&self) -> AllPoolTransactions<Self::Transaction> {
        let mut transactions = self.protocol_pool.all_transactions();
        let nonce_pool = self.nonce_pool.read();
        transactions.pending.extend(nonce_pool.pending_transactions());
        transactions.queued.extend(nonce_pool.queued_transactions());
        transactions
    }

    fn all_transaction_hashes(&self) -> Vec<TxHash> {
        let mut hashes = self.protocol_pool.all_transaction_hashes();
        hashes.extend(self.nonce_pool.read().all_hashes());
        hashes
    }

    fn remove_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let (protocol_hashes, sidecar_hashes) = self.partition_hashes_by_pool(hashes);
        let mut removed = self.protocol_pool.remove_transactions(protocol_hashes);
        self.untrack_eip8130_replays(&removed);
        let sidecar_removed = self.nonce_pool.write().remove_transactions(&sidecar_hashes);
        if !sidecar_removed.is_empty() {
            self.untrack_eip8130_replays(&sidecar_removed);
            self.listeners.write().on_discarded(&sidecar_removed);
        }
        removed.extend(sidecar_removed);
        removed
    }

    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let (protocol_hashes, sidecar_hashes) = self.partition_hashes_by_pool(hashes);
        let mut removed = self.protocol_pool.remove_transactions_and_descendants(protocol_hashes);
        self.untrack_eip8130_replays(&removed);
        let sidecar_removed =
            self.nonce_pool.write().remove_transactions_and_descendants(&sidecar_hashes);
        if !sidecar_removed.is_empty() {
            self.untrack_eip8130_replays(&sidecar_removed);
            self.listeners.write().on_discarded(&sidecar_removed);
        }
        removed.extend(sidecar_removed);
        removed
    }

    fn remove_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut removed = self.protocol_pool.remove_transactions_by_sender(sender);
        self.untrack_eip8130_replays(&removed);
        let sidecar_removed = self.nonce_pool.write().remove_transactions_by_sender(sender);
        if !sidecar_removed.is_empty() {
            self.untrack_eip8130_replays(&sidecar_removed);
            self.listeners.write().on_discarded(&sidecar_removed);
        }
        removed.extend(sidecar_removed);
        removed
    }

    fn prune_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let (protocol_hashes, sidecar_hashes) = self.partition_hashes_by_pool(hashes);
        let mut removed = self.protocol_pool.prune_transactions(protocol_hashes);
        self.untrack_eip8130_replays(&removed);
        let pruned = self.nonce_pool.write().prune_mined(&sidecar_hashes);
        self.untrack_eip8130_replays(&pruned.removed);
        removed.extend(pruned.removed);
        removed
    }

    fn retain_unknown<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData,
    {
        self.protocol_pool.retain_unknown(announcement);
        if announcement.is_empty() {
            return;
        }

        let nonce_pool = self.nonce_pool.read();
        announcement.retain_by_hash(|hash| !nonce_pool.contains(hash));
    }

    fn retain_contains<A>(&self, announcement: &mut A)
    where
        A: HandleMempoolData,
    {
        let nonce_pool = self.nonce_pool.read();
        announcement.retain_by_hash(|hash| {
            self.protocol_pool.get(hash).is_some() || nonce_pool.contains(hash)
        });
    }

    fn get(&self, tx_hash: &TxHash) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.protocol_pool.get(tx_hash).or_else(|| self.nonce_pool.read().get(tx_hash))
    }

    fn get_all(&self, txs: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let nonce_pool = self.nonce_pool.read();
        txs.into_iter()
            .filter_map(|tx| self.protocol_pool.get(&tx).or_else(|| nonce_pool.get(&tx)))
            .collect()
    }

    fn on_propagated(&self, txs: PropagatedTransactions) {
        let nonce_pool = self.nonce_pool.read();
        let protocol_txs = PropagatedTransactions(
            txs.0.into_iter().filter(|(hash, _)| !nonce_pool.contains(hash)).collect(),
        );
        drop(nonce_pool);
        self.protocol_pool.on_propagated(protocol_txs)
    }

    fn get_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_transactions_by_sender(sender);
        transactions.extend(self.nonce_pool.read().transactions_by_sender(sender));
        transactions
    }

    fn get_pending_transactions_with_predicate(
        &self,
        mut predicate: impl FnMut(&ValidPoolTransaction<Self::Transaction>) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions =
            self.protocol_pool.get_pending_transactions_with_predicate(&mut predicate);
        transactions.extend(
            self.nonce_pool
                .read()
                .pending_transactions()
                .into_iter()
                .filter(|transaction| predicate(transaction)),
        );
        transactions
    }

    fn get_pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_pending_transactions_by_sender(sender);
        transactions.extend(self.nonce_pool.read().pending_transactions_by_sender(sender));
        transactions
    }

    fn get_queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_queued_transactions_by_sender(sender);
        transactions.extend(self.nonce_pool.read().queued_transactions_by_sender(sender));
        transactions
    }

    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
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
        let mut transactions = self.protocol_pool.get_transactions_by_origin(origin);
        transactions.extend(
            self.nonce_pool
                .read()
                .all_transactions()
                .into_iter()
                .filter(|transaction| transaction.origin == origin),
        );
        transactions
    }

    fn get_pending_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut transactions = self.protocol_pool.get_pending_transactions_by_origin(origin);
        transactions.extend(
            self.nonce_pool
                .read()
                .pending_transactions()
                .into_iter()
                .filter(|transaction| transaction.origin == origin),
        );
        transactions
    }

    fn unique_senders(&self) -> AddressSet {
        let mut senders = self.protocol_pool.unique_senders();
        for sender in self.nonce_pool.read().unique_senders() {
            senders.insert(sender);
        }
        senders
    }

    fn get_blob(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        self.protocol_pool.get_blob(tx_hash)
    }

    fn get_all_blobs(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<(TxHash, Arc<BlobTransactionSidecarVariant>)>, BlobStoreError> {
        self.protocol_pool.get_all_blobs(tx_hashes)
    }

    fn get_all_blobs_exact(
        &self,
        tx_hashes: Vec<TxHash>,
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

    fn get_blobs_for_versioned_hashes_v4(
        &self,
        versioned_hashes: &[B256],
        indices_bitarray: B128,
    ) -> Result<Vec<Option<BlobCellsAndProofsV1>>, BlobStoreError> {
        self.protocol_pool.get_blobs_for_versioned_hashes_v4(versioned_hashes, indices_bitarray)
    }

    fn blob_store(&self) -> Box<dyn BlobStore> {
        Box::new(self.protocol_pool.blob_store().clone())
    }
}

impl<Client, S, Evm, T, O> TransactionPoolExt for BaseTransactionPool<Client, S, Evm, T, O>
where
    Client: 'static,
    Evm: 'static,
    BaseTransactionValidator<Client, T, Evm>: TransactionValidator<Transaction = T>,
    T: BasePooledTx + reth_transaction_pool::EthPoolTransaction + 'static,
    O: reth_transaction_pool::TransactionOrdering<Transaction = T> + Clone,
    S: BlobStore + Clone,
{
    type Block = <TransactionValidationTaskExecutor<BaseTransactionValidator<Client, T, Evm>> as TransactionValidator>::Block;

    fn set_block_info(&self, info: BlockInfo) {
        self.protocol_pool.set_block_info(info)
    }

    fn on_canonical_state_change(
        &self,
        update: reth_transaction_pool::CanonicalStateUpdate<'_, Self::Block>,
    ) {
        let block_hash = update.hash();
        let mined_transactions = update.mined_transactions.clone();
        self.untrack_eip8130_hashes(&mined_transactions);
        self.protocol_pool.on_canonical_state_change(update);
        let mut nonce_pool = self.nonce_pool.write();
        let pruned = nonce_pool.prune_mined(&mined_transactions);
        let mut listeners = self.listeners.write();
        if !pruned.removed.is_empty() {
            listeners.on_mined(&pruned.removed, block_hash);
        }
    }

    fn update_accounts(&self, accounts: Vec<ChangedAccount>) {
        let removed = self.nonce_pool.write().remove_unaffordable(&accounts);
        if !removed.is_empty() {
            self.untrack_eip8130_replays(&removed);
            self.listeners.write().on_discarded(&removed);
        }
        self.protocol_pool.update_accounts(accounts)
    }

    fn delete_blob(&self, tx: B256) {
        self.protocol_pool.delete_blob(tx)
    }

    fn delete_blobs(&self, txs: Vec<B256>) {
        self.protocol_pool.delete_blobs(txs)
    }

    fn cleanup_blobs(&self) {
        self.protocol_pool.cleanup_blobs()
    }
}

#[derive(Debug)]
struct SidecarListeners<T: BasePooledTx> {
    by_hash: HashMap<TxHash, Vec<mpsc::UnboundedSender<TransactionEvent>>>,
    all_events: Vec<mpsc::Sender<FullTransactionEvent<T>>>,
    pending_all: Vec<mpsc::Sender<TxHash>>,
    pending_propagate: Vec<mpsc::Sender<TxHash>>,
    new_all: Vec<mpsc::Sender<NewTransactionEvent<T>>>,
    new_propagate: Vec<mpsc::Sender<NewTransactionEvent<T>>>,
}

impl<T: BasePooledTx> Default for SidecarListeners<T> {
    fn default() -> Self {
        Self {
            by_hash: HashMap::new(),
            all_events: Vec::new(),
            pending_all: Vec::new(),
            pending_propagate: Vec::new(),
            new_all: Vec::new(),
            new_propagate: Vec::new(),
        }
    }
}

impl<T: BasePooledTx> SidecarListeners<T> {
    fn subscribe_hash(
        &mut self,
        tx_hash: TxHash,
    ) -> (TransactionEvents, mpsc::UnboundedSender<TransactionEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.by_hash.entry(tx_hash).or_default().push(tx.clone());
        (TransactionEvents::new(tx_hash, rx), tx)
    }

    fn unsubscribe_hash_listener(
        &mut self,
        tx_hash: &TxHash,
        listener: &mpsc::UnboundedSender<TransactionEvent>,
    ) {
        let Some(listeners) = self.by_hash.get_mut(tx_hash) else {
            return;
        };
        listeners.retain(|candidate| !candidate.same_channel(listener));
        if listeners.is_empty() {
            self.by_hash.remove(tx_hash);
        }
    }

    fn subscribe_all(&mut self) -> AllTransactionsEvents<T> {
        let (tx, rx) = mpsc::channel(SIDE_CAR_EVENT_CHANNEL_SIZE);
        self.all_events.push(tx);
        AllTransactionsEvents::new(rx)
    }

    fn subscribe_pending(&mut self, kind: TransactionListenerKind) -> mpsc::Receiver<TxHash> {
        let (tx, rx) = mpsc::channel(SIDE_CAR_EVENT_CHANNEL_SIZE);
        if kind.is_propagate_only() {
            self.pending_propagate.push(tx);
        } else {
            self.pending_all.push(tx);
        }
        rx
    }

    fn subscribe_new_transactions(
        &mut self,
        kind: TransactionListenerKind,
    ) -> mpsc::Receiver<NewTransactionEvent<T>> {
        let (tx, rx) = mpsc::channel(SIDE_CAR_EVENT_CHANNEL_SIZE);
        if kind.is_propagate_only() {
            self.new_propagate.push(tx);
        } else {
            self.new_all.push(tx);
        }
        rx
    }

    fn on_inserted(&mut self, nonce_pool: &TwoDNoncePool<T>, outcome: &InsertOutcome<T>) {
        let hash = outcome.outcome.hash;
        let Some(transaction) = nonce_pool.get(&hash) else {
            return;
        };

        if let Some(replaced) = &outcome.replaced {
            self.broadcast_hash_event(replaced.hash(), TransactionEvent::Replaced(hash));
            self.broadcast_all(FullTransactionEvent::Replaced {
                transaction: Arc::clone(replaced),
                replaced_by: hash,
            });
        }

        match &outcome.outcome.state {
            AddedTransactionState::Pending => {
                self.broadcast_pending_transaction(&transaction);
            }
            AddedTransactionState::Queued(reason) => {
                self.broadcast_hash_event(&hash, TransactionEvent::Queued);
                self.broadcast_all(FullTransactionEvent::Queued(hash, Some(reason.clone())));
                self.broadcast_new(NewTransactionEvent { subpool: SubPool::Queued, transaction });
            }
        }

        for promoted in &outcome.promoted {
            self.broadcast_pending_transaction(promoted);
        }
    }

    fn on_mined(&mut self, transactions: &[Arc<ValidPoolTransaction<T>>], block_hash: B256) {
        for transaction in transactions {
            let hash = *transaction.hash();
            self.broadcast_hash_event(&hash, TransactionEvent::Mined(block_hash));
            self.broadcast_all(FullTransactionEvent::Mined { tx_hash: hash, block_hash });
        }
    }

    fn on_discarded(&mut self, transactions: &[Arc<ValidPoolTransaction<T>>]) {
        for transaction in transactions {
            let hash = *transaction.hash();
            self.broadcast_hash_event(&hash, TransactionEvent::Discarded);
            self.broadcast_all(FullTransactionEvent::Discarded(hash));
        }
    }

    fn broadcast_hash_event(&mut self, tx_hash: &TxHash, event: TransactionEvent) {
        let Some(listeners) = self.by_hash.get_mut(tx_hash) else {
            return;
        };
        listeners.retain(|listener| listener.send(event.clone()).is_ok() && !event.is_final());
        if listeners.is_empty() {
            self.by_hash.remove(tx_hash);
        }
    }

    fn broadcast_all(&mut self, event: FullTransactionEvent<T>) {
        self.all_events.retain(|listener| listener.try_send(event.clone()).is_ok());
    }

    fn broadcast_pending_transaction(&mut self, transaction: &Arc<ValidPoolTransaction<T>>) {
        let hash = *transaction.hash();
        self.broadcast_hash_event(&hash, TransactionEvent::Pending);
        self.broadcast_all(FullTransactionEvent::Pending(hash));
        self.broadcast_pending(transaction);
        self.broadcast_new(NewTransactionEvent::pending(Arc::clone(transaction)));
    }

    fn broadcast_pending(&mut self, transaction: &Arc<ValidPoolTransaction<T>>) {
        self.pending_all.retain(|listener| listener.try_send(*transaction.hash()).is_ok());
        if transaction.propagate {
            self.pending_propagate
                .retain(|listener| listener.try_send(*transaction.hash()).is_ok());
        }
    }

    fn broadcast_new(&mut self, event: NewTransactionEvent<T>) {
        self.new_all.retain(|listener| listener.try_send(event.clone()).is_ok());
        if event.transaction.propagate {
            let propagate_event = event.clone();
            self.new_propagate
                .retain(|listener| listener.try_send(propagate_event.clone()).is_ok());
        }
    }
}

fn merge_receivers<T: Send + 'static>(
    mut left: mpsc::Receiver<T>,
    mut right: mpsc::Receiver<T>,
) -> mpsc::Receiver<T> {
    let (tx, rx) = mpsc::channel(SIDE_CAR_EVENT_CHANNEL_SIZE);
    spawn(async move {
        let mut left_open = true;
        let mut right_open = true;
        while left_open || right_open {
            tokio::select! {
                item = left.recv(), if left_open => match item {
                    Some(item) => {
                        if tx.send(item).await.is_err() {
                            break;
                        }
                    }
                    None => left_open = false,
                },
                item = right.recv(), if right_open => match item {
                    Some(item) => {
                        if tx.send(item).await.is_err() {
                            break;
                        }
                    }
                    None => right_open = false,
                }
            }
        }
    });
    rx
}

fn pooled_element<T: BasePooledTx>(
    transaction: &Arc<ValidPoolTransaction<T>>,
) -> Option<(<T as PoolTransaction>::Pooled, usize)> {
    let encoded_length = transaction.encoded_length();
    transaction
        .transaction
        .clone()
        .try_into_pooled()
        .ok()
        .map(|recovered| recovered.into_parts().0)
        .map(|pooled| (pooled, encoded_length))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use alloy_consensus::{Transaction, transaction::Recovered};
    use alloy_primitives::{Bytes, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, Eip8130Signed, TxEip8130,
    };
    use futures::StreamExt;
    use reth_transaction_pool::{PriceBumpConfig, TransactionOrigin, identifier::TransactionId};

    use super::*;
    use crate::BasePooledTransaction;

    fn test_chain_id() -> u64 {
        ChainConfig::mainnet().chain_id
    }

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::random()
    }

    fn signed_channel_tx(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key,
            nonce_sequence,
            expiry: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed =
            Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()))
    }

    fn valid_pool_transaction(
        transaction: BasePooledTransaction,
    ) -> ValidPoolTransaction<BasePooledTransaction> {
        ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), transaction.nonce()),
            transaction,
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        }
    }

    #[tokio::test]
    async fn hash_subscriber_receives_initial_pending_event_for_new_sidecar_transaction() {
        let mut nonce_pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let mut listeners = SidecarListeners::default();
        let signer = signer();

        let transaction =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(1), 0, 1_000));
        let hash = *transaction.hash();

        let mut events = listeners.subscribe_hash(hash).0;
        let outcome = nonce_pool.insert_validated(transaction, 0).unwrap();
        listeners.on_inserted(&nonce_pool, &outcome);

        assert!(matches!(events.next().await, Some(TransactionEvent::Pending)));
    }

    #[tokio::test]
    async fn gap_fill_broadcasts_pending_for_promoted_sidecar_transaction() {
        let mut nonce_pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let mut listeners = SidecarListeners::default();
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(1), 0, 1_000));
        let queued = valid_pool_transaction(signed_channel_tx(&signer, U256::from(1), 2, 800));
        let queued_hash = *queued.hash();
        let middle = valid_pool_transaction(signed_channel_tx(&signer, U256::from(1), 1, 900));
        let middle_hash = *middle.hash();

        nonce_pool.insert_validated(first, 0).unwrap();
        nonce_pool.insert_validated(queued, 0).unwrap();

        let mut pending = listeners.subscribe_pending(TransactionListenerKind::All);
        let mut queued_events = listeners.subscribe_hash(queued_hash).0;

        let outcome = nonce_pool.insert_validated(middle, 0).unwrap();
        listeners.on_inserted(&nonce_pool, &outcome);

        assert_eq!(pending.recv().await, Some(middle_hash));
        assert_eq!(pending.recv().await, Some(queued_hash));
        assert!(matches!(queued_events.next().await, Some(TransactionEvent::Pending)));
    }

    #[tokio::test]
    async fn discarded_sidecar_transaction_broadcasts_terminal_events() {
        let mut listeners = SidecarListeners::default();
        let signer = signer();
        let transaction =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(2), 0, 1_000)));
        let hash = *transaction.hash();

        let mut hash_events = listeners.subscribe_hash(hash).0;
        let mut all_events = listeners.subscribe_all();

        listeners.on_discarded(&[transaction]);

        assert!(matches!(hash_events.next().await, Some(TransactionEvent::Discarded)));
        assert!(
            matches!(all_events.next().await, Some(FullTransactionEvent::Discarded(event_hash)) if event_hash == hash)
        );
    }
}
