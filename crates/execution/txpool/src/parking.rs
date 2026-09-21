//! Lane-aware parking over an existing best-transactions iterator.

use std::{
    collections::{BinaryHeap, VecDeque},
    sync::Arc,
};

use alloy_primitives::{
    Address, TxHash, U256,
    map::{HashMap, HashSet, hash_map::Entry},
};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionOrdering,
    TransactionPool, ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolTransactionError},
};

use crate::{BasePooledTx, BestTransactionPriority};

/// Excludes a transaction from the remainder of a best-transactions iterator
/// because its EIP-8130 gas payer was suspended: the payer's balance can no
/// longer cover its sponsored transactions.
///
/// This is not a bad transaction. The payer may refund before a later build, so
/// the transaction stays in the pool and can be re-selected; only the current
/// iterator excludes it.
#[derive(Debug, thiserror::Error)]
#[error("EIP-8130 gas payer suspended during selection")]
pub struct PayerSuspended;

impl PoolTransactionError for PayerSuspended {
    fn is_bad_transaction(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// A sequential transaction lane whose members must execute in nonce order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BestTransactionLane {
    /// Transaction sender whose nonces define the lane.
    pub sender: Address,
    /// Nonce channel key, with zero representing the protocol nonce lane.
    pub nonce_key: U256,
}

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

impl BestTransactionLane {
    /// Returns the sequential lane for a transaction, or `None` for an independent nonce-free
    /// EIP-8130 transaction.
    pub fn for_transaction<T>(transaction: &Arc<ValidPoolTransaction<T>>) -> Option<Self>
    where
        T: BasePooledTx,
    {
        let nonce_key = transaction.transaction.eip8130_nonce_channel_key();
        if nonce_key.is_none() && transaction.transaction.eip8130_replay_id().is_some() {
            return None;
        }
        Some(Self { sender: transaction.sender(), nonce_key: nonce_key.unwrap_or_default() })
    }
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

    /// Suspends an EIP-8130 gas payer, excluding every remaining transaction it
    /// funds from the rest of this iterator.
    ///
    /// The flag itself is set in O(1) and covers all of the payer's sponsored
    /// transactions regardless of how many there are; each surfacing candidate
    /// is then skipped lazily as it is selected, and the first skip of a lane
    /// drops that lane's blocked descendants with it. Idempotent.
    fn suspend_payer(&mut self, payer: Address);
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
    lanes: HashMap<BestTransactionLane, BestTransactionLaneState<T>>,
    parked: HashMap<TxHash, Arc<ValidPoolTransaction<T>>>,
    ready: HashMap<TxHash, Arc<ValidPoolTransaction<T>>>,
    ready_heap: BinaryHeap<(BestTransactionPriority<O::PriorityValue>, TxHash)>,
    suspended_payers: HashSet<Address>,
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
            suspended_payers: HashSet::default(),
        }
    }

    /// Returns whether this transaction's EIP-8130 gas payer is suspended.
    fn is_gas_payer_suspended(&self, transaction: &Arc<ValidPoolTransaction<T>>) -> bool {
        transaction
            .transaction
            .gas_payer()
            .is_some_and(|payer| self.suspended_payers.contains(&payer))
    }

    /// Excludes a suspended-payer transaction from the remainder of the iterator.
    ///
    /// A finite-channel transaction terminally invalidates its lane, which drops
    /// its blocked descendants; a nonce-free transaction is dropped on its own.
    /// The source iterator is notified so it excludes the lane's descendants too.
    fn exclude_suspended(&mut self, transaction: Arc<ValidPoolTransaction<T>>) {
        if let Some(lane) = BestTransactionLane::for_transaction(&transaction) {
            self.invalidate_lane(lane);
        } else {
            let hash = *transaction.hash();
            self.ready.remove(&hash);
            self.parked.remove(&hash);
        }
        self.inner
            .mark_invalid(&transaction, InvalidPoolTransactionError::other(PayerSuspended));
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
    pub fn release_lane(&mut self, lane: BestTransactionLane) {
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
    pub fn invalidate_lane(&mut self, lane: BestTransactionLane) {
        self.lanes.insert(lane, BestTransactionLaneState::Invalid);
        if self
            .source_head
            .as_ref()
            .and_then(BestTransactionLane::for_transaction)
            .is_some_and(|head_lane| head_lane == lane)
        {
            self.source_head = None;
        }
        self.parked.retain(|_, transaction| {
            BestTransactionLane::for_transaction(transaction) != Some(lane)
        });
        self.ready.retain(|_, transaction| {
            BestTransactionLane::for_transaction(transaction) != Some(lane)
        });
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
            let Some(lane) = BestTransactionLane::for_transaction(&transaction) else {
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
        if let Some(lane) = BestTransactionLane::for_transaction(&transaction) {
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
        loop {
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

            // Lazily skip candidates whose gas payer was suspended. The first
            // skip of a lane invalidates it, dropping its blocked descendants.
            if self.is_gas_payer_suspended(&transaction) {
                self.exclude_suspended(transaction);
                continue;
            }

            return Some(self.record_yielded(transaction));
        }
    }
}

impl<T, I, O> BestTransactions for ParkedBestTransactions<T, I, O>
where
    T: BasePooledTx,
    I: BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    O: TransactionOrdering<Transaction = T>,
{
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError) {
        if let Some(lane) = BestTransactionLane::for_transaction(transaction) {
            self.invalidate_lane(lane);
        }
        self.inner.mark_invalid(transaction, kind);
    }

    fn no_updates(&mut self) {
        self.inner.no_updates();
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
        if let Some(lane) = BestTransactionLane::for_transaction(&transaction) {
            self.invalidate_lane(lane);
        }
        self.inner.mark_invalid(&transaction, kind);
        true
    }

    fn mark_committed(&mut self, transaction: &Arc<ValidPoolTransaction<T>>) {
        if let Some(lane) = BestTransactionLane::for_transaction(transaction) {
            self.release_lane(lane);
        }
    }

    fn suspend_payer(&mut self, payer: Address) {
        // The flag is the only mandatory work: it is O(1) and covers every
        // transaction this payer funds, now and as they surface. `next` skips
        // them lazily. Purge candidates already staged so we neither yield nor
        // rank them: draining a payer must not keep its transactions competing.
        if !self.suspended_payers.insert(payer) {
            return;
        }
        let source_suspended = self
            .source_head
            .as_ref()
            .is_some_and(|transaction| self.is_gas_payer_suspended(transaction));
        if source_suspended {
            let transaction = self.source_head.take().expect("source head checked above");
            self.exclude_suspended(transaction);
        }
        let staged: Vec<Arc<ValidPoolTransaction<T>>> = self
            .ready
            .values()
            .chain(self.parked.values())
            .filter(|transaction| self.is_gas_payer_suspended(transaction))
            .map(Arc::clone)
            .collect();
        for transaction in staged {
            self.exclude_suspended(transaction);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, time::Instant};

    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::Bytes;
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
            let lane = BestTransactionLane::for_transaction(transaction);
            self.transactions.retain(|candidate| {
                BestTransactionLane::for_transaction(candidate) != lane || lane.is_none()
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

    /// Builds an EIP-8130 transaction whose gas is paid by an explicit `payer`,
    /// distinct from the sender, to model a sponsorship (1:Many) lane.
    fn sponsored_transaction(
        signer: &PrivateKeySigner,
        payer: Address,
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
            payer: Some(payer),
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
        let lane = BestTransactionLane::for_transaction(&transaction).unwrap();
        let inner = StaticBestTransactions::new(Vec::new());
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        best.push_ready(transaction);
        best.invalidate_lane(lane);

        assert!(best.ready.is_empty());
        assert!(best.ready_heap.is_empty());
    }

    #[test]
    fn suspend_payer_excludes_all_of_that_payers_sponsored_transactions() {
        let payer = Address::repeat_byte(0xAA);
        let sender_a = PrivateKeySigner::random();
        let sender_b = PrivateKeySigner::random();
        let sender_c = PrivateKeySigner::random();
        let self_payer = PrivateKeySigner::random();
        let sponsored_a = sponsored_transaction(&sender_a, payer, U256::ZERO, 0, 100);
        let sponsored_b = sponsored_transaction(&sender_b, payer, U256::ZERO, 0, 90);
        let sponsored_c = sponsored_transaction(&sender_c, payer, U256::ZERO, 0, 80);
        let unrelated = transaction(&self_payer, U256::ZERO, 0, 10);

        let inner = StaticBestTransactions::new(vec![
            Arc::clone(&sponsored_a),
            Arc::clone(&sponsored_b),
            Arc::clone(&sponsored_c),
            Arc::clone(&unrelated),
        ]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        // One flag drop revokes every lane the payer funds, across all senders.
        best.suspend_payer(payer);

        assert_eq!(best.next().unwrap().hash(), unrelated.hash());
        assert!(best.next().is_none());
    }

    #[test]
    fn suspend_payer_mid_iteration_skips_the_payers_remaining_transactions() {
        let payer = Address::repeat_byte(0xBB);
        let sender_a = PrivateKeySigner::random();
        let sender_b = PrivateKeySigner::random();
        let self_payer = PrivateKeySigner::random();
        let first = sponsored_transaction(&sender_a, payer, U256::ZERO, 0, 100);
        let second = sponsored_transaction(&sender_b, payer, U256::ZERO, 0, 90);
        let unrelated = transaction(&self_payer, U256::ZERO, 0, 10);

        let inner = StaticBestTransactions::new(vec![
            Arc::clone(&first),
            Arc::clone(&second),
            Arc::clone(&unrelated),
        ]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        // The highest-priority sponsored transaction is included, then the payer
        // drains mid-build and the rest of its book is skipped.
        assert_eq!(best.next().unwrap().hash(), first.hash());
        best.mark_committed(&first);
        best.suspend_payer(payer);

        assert_eq!(best.next().unwrap().hash(), unrelated.hash());
        assert!(best.next().is_none());
    }

    #[test]
    fn suspend_payer_covers_a_self_paying_sender() {
        let signer = PrivateKeySigner::random();
        let other = PrivateKeySigner::random();
        let self_pay = transaction(&signer, U256::ZERO, 0, 100);
        let unrelated = transaction(&other, U256::ZERO, 0, 10);

        let inner =
            StaticBestTransactions::new(vec![Arc::clone(&self_pay), Arc::clone(&unrelated)]);
        let mut best = ParkedBestTransactions::new(inner, BaseOrdering::coinbase_tip(), 0);

        // A self-paying sender's gas payer is itself, so suspending it excludes
        // its transactions just like a sponsor.
        best.suspend_payer(signer.address());

        assert_eq!(best.next().unwrap().hash(), unrelated.hash());
        assert!(best.next().is_none());
    }
}
