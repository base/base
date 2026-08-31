//! Lane-aware parking over an existing best-transactions iterator.

use std::{
    collections::{BinaryHeap, VecDeque},
    sync::Arc,
};

use alloy_primitives::{
    TxHash,
    map::{HashMap, hash_map::Entry},
};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionOrdering,
    TransactionPool, ValidPoolTransaction, error::InvalidPoolTransactionError,
};

use crate::{BasePooledTx, BaseTransactionLane, BestTransactionPriority};

/// Iteration-local state for a sequential transaction lane.
#[derive(Debug)]
pub enum BestTransactionLaneState<T>
where
    T: PoolTransaction,
{
    /// The lane has an unresolved head and buffers any descendants yielded by the source.
    Occupied(VecDeque<Arc<ValidPoolTransaction<T>>>),
    /// The lane was terminally invalidated and is excluded for the remainder of the iterator.
    Invalid,
}

/// Extra lifecycle operations required to temporarily park best transactions.
pub trait ParkableBestTransactions<T>:
    BestTransactions<Item = Arc<ValidPoolTransaction<T>>>
where
    T: PoolTransaction,
{
    /// Temporarily parks a transaction that was yielded by this iterator.
    fn park(&mut self, transaction: &Arc<ValidPoolTransaction<T>>);

    /// Makes a parked transaction eligible to compete by priority again.
    fn promote(&mut self, transaction_hash: TxHash) -> bool;

    /// Invalidates a parked transaction for the remainder of this iterator.
    fn discard_parked(
        &mut self,
        transaction_hash: TxHash,
        kind: InvalidPoolTransactionError,
    ) -> bool;

    /// Records that a yielded transaction committed and releases its lane successor.
    fn mark_committed(&mut self, transaction: &Arc<ValidPoolTransaction<T>>);
}

/// A transaction pool that can create lane-aware parkable best iterators.
pub trait ParkableTransactionPool: TransactionPool
where
    Self::Transaction: BasePooledTx,
{
    /// Returns a parkable best iterator using the supplied fee attributes.
    fn best_transactions_with_attributes_and_parking(
        &self,
        attributes: BestTransactionsAttributes,
    ) -> Box<dyn ParkableBestTransactions<Self::Transaction>>;
}

/// Lane-aware parking over an existing best-transactions iterator.
///
/// The inner iterator remains responsible for nonce contiguity and source ordering. This adapter
/// only buffers descendants that the inner iterator unlocks while an earlier member of their lane
/// is parked or waiting for an execution outcome.
pub struct ParkedBestTransactions<T, I, O>
where
    T: BasePooledTx,
    I: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    O: TransactionOrdering<Transaction = T>,
{
    inner: I,
    ordering: O,
    base_fee: u64,
    source_head: Option<Arc<ValidPoolTransaction<T>>>,
    lanes: HashMap<BaseTransactionLane, BestTransactionLaneState<T>>,
    parked: HashMap<TxHash, Arc<ValidPoolTransaction<T>>>,
    ready: HashMap<TxHash, Arc<ValidPoolTransaction<T>>>,
    ready_heap: BinaryHeap<(BestTransactionPriority<O::PriorityValue>, TxHash)>,
}

impl<T, I, O> std::fmt::Debug for ParkedBestTransactions<T, I, O>
where
    T: BasePooledTx,
    I: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    O: TransactionOrdering<Transaction = T>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParkedBestTransactions")
            .field("lanes", &self.lanes.len())
            .field("parked", &self.parked.len())
            .field("ready", &self.ready.len())
            .finish_non_exhaustive()
    }
}

impl<T, I, O> ParkedBestTransactions<T, I, O>
where
    T: BasePooledTx,
    I: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    O: TransactionOrdering<Transaction = T>,
{
    /// Creates a lane-aware parking adapter.
    pub fn new(inner: I, ordering: O, base_fee: u64) -> Self {
        Self {
            inner,
            ordering,
            base_fee,
            source_head: None,
            lanes: HashMap::default(),
            parked: HashMap::default(),
            ready: HashMap::default(),
            ready_heap: BinaryHeap::new(),
        }
    }

    /// Returns a complete priority key for a transaction.
    pub fn priority(
        &self,
        transaction: &Arc<ValidPoolTransaction<T>>,
    ) -> BestTransactionPriority<O::PriorityValue> {
        BestTransactionPriority::new(&self.ordering, transaction, self.base_fee)
    }

    /// Adds a transaction to the priority-ordered ready set.
    pub fn push_ready(&mut self, transaction: Arc<ValidPoolTransaction<T>>) {
        let hash = *transaction.hash();
        if self.ready.contains_key(&hash) {
            return;
        }
        let priority = self.priority(&transaction);
        self.ready.insert(hash, transaction);
        self.ready_heap.push((priority, hash));
    }

    /// Releases the next buffered transaction in a committed lane.
    pub fn release_lane(&mut self, lane: BaseTransactionLane) {
        let next = match self.lanes.entry(lane) {
            Entry::Occupied(mut entry) => match entry.get_mut() {
                BestTransactionLaneState::Occupied(buffered) => {
                    let next = buffered.pop_front();
                    if next.is_none() {
                        entry.remove();
                    }
                    next
                }
                BestTransactionLaneState::Invalid => return,
            },
            Entry::Vacant(_) => return,
        };
        if let Some(next) = next {
            self.push_ready(next);
        }
    }

    /// Discards all locally tracked members of a terminally invalid lane.
    ///
    /// Buffered descendants have already been consumed from `inner`. Invalidating the yielded lane
    /// head notifies `inner`, whose lane bookkeeping excludes its remaining descendants.
    pub fn invalidate_lane(&mut self, lane: BaseTransactionLane) {
        self.lanes.insert(lane, BestTransactionLaneState::Invalid);
        if self
            .source_head
            .as_ref()
            .and_then(|transaction| transaction.transaction.identity().lane())
            .is_some_and(|head_lane| head_lane == lane)
        {
            self.source_head = None;
        }
        self.parked
            .retain(|_, transaction| transaction.transaction.identity().lane() != Some(lane));
        self.ready.retain(|_, transaction| transaction.transaction.identity().lane() != Some(lane));
        self.ready_heap.retain(|(_, hash)| self.ready.contains_key(hash));
    }

    /// Pulls through blocked descendants until the next source candidate is lane-eligible.
    ///
    /// Consuming occupied-lane descendants is the intentionally naive BASE-253 implementation:
    /// the source only exposes other lanes after these transactions are pulled and buffered. The
    /// `Invalid` arm is defensive because terminal invalidation is already forwarded to `inner`,
    /// whose [`BestTransactions`] contract excludes the invalid transaction's descendants.
    pub fn fill_source_head(&mut self) {
        while self.source_head.is_none() {
            let Some(transaction) = self.inner.next() else {
                return;
            };
            let Some(lane) = transaction.transaction.identity().lane() else {
                self.source_head = Some(transaction);
                return;
            };
            match self.lanes.get_mut(&lane) {
                Some(BestTransactionLaneState::Invalid) => continue,
                Some(BestTransactionLaneState::Occupied(buffered)) => {
                    buffered.push_back(transaction);
                    continue;
                }
                None => {}
            }
            self.source_head = Some(transaction);
        }
    }

    /// Removes stale heap entries and returns the highest-priority ready key.
    pub fn ready_priority(&mut self) -> Option<&BestTransactionPriority<O::PriorityValue>> {
        while self.ready_heap.peek().is_some_and(|(_, hash)| !self.ready.contains_key(hash)) {
            self.ready_heap.pop();
        }
        self.ready_heap.peek().map(|(priority, _)| priority)
    }

    /// Pops the highest-priority non-stale ready transaction.
    pub fn pop_ready(&mut self) -> Option<Arc<ValidPoolTransaction<T>>> {
        loop {
            let (_, hash) = self.ready_heap.pop()?;
            if let Some(transaction) = self.ready.remove(&hash) {
                return Some(transaction);
            }
        }
    }

    /// Records a transaction as yielded and occupies its sequential lane.
    pub fn record_yielded(
        &mut self,
        transaction: Arc<ValidPoolTransaction<T>>,
    ) -> Arc<ValidPoolTransaction<T>> {
        if let Some(lane) = transaction.transaction.identity().lane() {
            self.lanes
                .entry(lane)
                .or_insert_with(|| BestTransactionLaneState::Occupied(VecDeque::new()));
        }
        transaction
    }
}

impl<T, I, O> Iterator for ParkedBestTransactions<T, I, O>
where
    T: BasePooledTx,
    I: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    O: TransactionOrdering<Transaction = T>,
{
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.fill_source_head();

        let ready_priority = self.ready_priority().cloned();
        let source_priority = self.source_head.as_ref().map(|source| self.priority(source));
        let take_ready = match (source_priority, ready_priority) {
            (Some(source), Some(ready)) => ready >= source,
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (None, None) => return None,
        };

        let transaction = if take_ready {
            self.pop_ready().expect("ready priority requires a ready transaction")
        } else {
            self.source_head.take().expect("source priority requires a source transaction")
        };
        Some(self.record_yielded(transaction))
    }
}

impl<T, I, O> BestTransactions for ParkedBestTransactions<T, I, O>
where
    T: BasePooledTx,
    I: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    O: TransactionOrdering<Transaction = T>,
{
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError) {
        if let Some(lane) = transaction.transaction.identity().lane() {
            self.invalidate_lane(lane);
        }
        self.inner.mark_invalid(transaction, kind);
    }

    fn no_updates(&mut self) {
        self.inner.no_updates();
    }

    fn allow_updates_out_of_order(&mut self) {
        self.inner.allow_updates_out_of_order();
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.inner.set_skip_blobs(skip_blobs);
    }
}

impl<T, I, O> ParkableBestTransactions<T> for ParkedBestTransactions<T, I, O>
where
    T: BasePooledTx,
    I: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    O: TransactionOrdering<Transaction = T>,
{
    fn park(&mut self, transaction: &Arc<ValidPoolTransaction<T>>) {
        let hash = *transaction.hash();
        self.parked.insert(hash, Arc::clone(transaction));
    }

    fn promote(&mut self, transaction_hash: TxHash) -> bool {
        let Some(transaction) = self.parked.remove(&transaction_hash) else {
            return false;
        };
        self.push_ready(transaction);
        true
    }

    fn discard_parked(
        &mut self,
        transaction_hash: TxHash,
        kind: InvalidPoolTransactionError,
    ) -> bool {
        let Some(transaction) = self.parked.remove(&transaction_hash) else {
            return false;
        };
        if let Some(lane) = transaction.transaction.identity().lane() {
            self.invalidate_lane(lane);
        }
        self.inner.mark_invalid(&transaction, kind);
        true
    }

    fn mark_committed(&mut self, transaction: &Arc<ValidPoolTransaction<T>>) {
        if let Some(lane) = transaction.transaction.identity().lane() {
            self.release_lane(lane);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, time::Instant};

    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Bytes, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, Eip8130Constants, Eip8130Signed,
        TxEip8130,
    };
    use reth_primitives_traits::Recovered;
    use reth_transaction_pool::{TransactionOrigin, identifier::TransactionId};

    use super::*;
    use crate::{BaseOrdering, BasePooledTransaction};

    #[derive(Debug)]
    struct StaticBestTransactions {
        transactions: VecDeque<Arc<ValidPoolTransaction<BasePooledTransaction>>>,
    }

    impl StaticBestTransactions {
        fn new(transactions: Vec<Arc<ValidPoolTransaction<BasePooledTransaction>>>) -> Self {
            Self { transactions: transactions.into() }
        }
    }

    impl Iterator for StaticBestTransactions {
        type Item = Arc<ValidPoolTransaction<BasePooledTransaction>>;

        fn next(&mut self) -> Option<Self::Item> {
            self.transactions.pop_front()
        }
    }

    impl BestTransactions for StaticBestTransactions {
        fn mark_invalid(&mut self, transaction: &Self::Item, _kind: InvalidPoolTransactionError) {
            let lane = transaction.transaction.identity().lane();
            self.transactions.retain(|candidate| {
                candidate.transaction.identity().lane() != lane || lane.is_none()
            });
        }

        fn no_updates(&mut self) {}

        fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
    }

    fn transaction(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce: u64,
        priority_fee: u128,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        let tx = TxEip8130 {
            chain_id: ChainConfig::mainnet().chain_id,
            sender: None,
            nonce_key,
            nonce_sequence: nonce,
            valid_after: 0,
            valid_before: u64::from(nonce_key == Eip8130Constants::NONCE_KEY_MAX),
            max_priority_fee_per_gas: priority_fee,
            max_fee_per_gas: priority_fee + 10,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed = Eip8130Signed::new(tx, Bytes::from(signature.as_bytes()), Bytes::new());
        let pooled = ConsensusPooledTransaction::Eip8130(signed);
        let encoded_length = pooled.encode_2718_len();
        let transaction = BasePooledTransaction::new(
            Recovered::new_unchecked(pooled.into(), signer.address()),
            encoded_length,
        );
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), nonce),
            transaction,
            propagate: true,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    #[test]
    fn parked_protocol_parent_releases_buffered_descendant_after_commit() {
        let parent_signer = PrivateKeySigner::random();
        let trigger_signer = PrivateKeySigner::random();
        let low_signer = PrivateKeySigner::random();
        let parent = transaction(&parent_signer, U256::ZERO, 0, 100);
        let child = transaction(&parent_signer, U256::ZERO, 1, 90);
        let trigger = transaction(&trigger_signer, U256::ZERO, 0, 50);
        let low = transaction(&low_signer, U256::ZERO, 0, 1);
        let parent_hash = *parent.hash();

        let inner = StaticBestTransactions::new(vec![
            Arc::clone(&parent),
            Arc::clone(&child),
            Arc::clone(&trigger),
            Arc::clone(&low),
        ]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        assert_eq!(best.next().unwrap().hash(), parent.hash());
        best.park(&parent);
        assert_eq!(best.next().unwrap().hash(), trigger.hash());
        best.mark_committed(&trigger);
        assert!(best.promote(parent_hash));
        assert_eq!(best.next().unwrap().hash(), parent.hash());
        best.mark_committed(&parent);
        assert_eq!(best.next().unwrap().hash(), child.hash());
        best.mark_committed(&child);
        assert_eq!(best.next().unwrap().hash(), low.hash());
    }

    #[test]
    fn finite_channels_and_nonce_free_transactions_are_independent() {
        let signer = PrivateKeySigner::random();
        let channel_one = U256::from(1);
        let channel_two = U256::from(2);
        let parent = transaction(&signer, channel_one, 0, 100);
        let child = transaction(&signer, channel_one, 1, 90);
        let other_channel = transaction(&signer, channel_two, 0, 50);
        let nonce_free = transaction(&signer, Eip8130Constants::NONCE_KEY_MAX, 0, 40);
        let parent_hash = *parent.hash();

        let inner = StaticBestTransactions::new(vec![
            Arc::clone(&parent),
            Arc::clone(&child),
            Arc::clone(&other_channel),
            Arc::clone(&nonce_free),
        ]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        assert_eq!(best.next().unwrap().hash(), parent.hash());
        best.park(&parent);
        assert_eq!(best.next().unwrap().hash(), other_channel.hash());
        best.mark_committed(&other_channel);
        assert_eq!(best.next().unwrap().hash(), nonce_free.hash());
        best.mark_committed(&nonce_free);
        assert!(best.promote(parent_hash));
        assert_eq!(best.next().unwrap().hash(), parent.hash());
        best.mark_committed(&parent);
        assert_eq!(best.next().unwrap().hash(), child.hash());
    }

    #[test]
    fn discarding_parked_head_excludes_its_lane_for_the_iteration() {
        let parent_signer = PrivateKeySigner::random();
        let other_signer = PrivateKeySigner::random();
        let parent = transaction(&parent_signer, U256::ZERO, 0, 100);
        let child = transaction(&parent_signer, U256::ZERO, 1, 90);
        let other = transaction(&other_signer, U256::ZERO, 0, 1);
        let parent_hash = *parent.hash();
        let inner =
            StaticBestTransactions::new(vec![Arc::clone(&parent), child, Arc::clone(&other)]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        assert_eq!(best.next().unwrap().hash(), parent.hash());
        best.park(&parent);
        assert!(best.discard_parked(parent_hash, InvalidPoolTransactionError::Underpriced));
        assert_eq!(best.next().unwrap().hash(), other.hash());
        assert!(best.next().is_none());
    }

    #[test]
    fn invalidating_lane_removes_its_ready_heap_entries() {
        let signer = PrivateKeySigner::random();
        let transaction = transaction(&signer, U256::from(1), 0, 100);
        let lane = transaction.transaction.identity().lane().unwrap();
        let inner = StaticBestTransactions::new(Vec::new());
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        best.push_ready(transaction);
        best.invalidate_lane(lane);

        assert!(best.ready.is_empty());
        assert!(best.ready_heap.is_empty());
    }
}
