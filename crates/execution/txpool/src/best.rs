//! Merged best-transaction iteration across the protocol pool, the EIP-8130
//! sidecar and the validity pool.

use std::{
    collections::{BinaryHeap, VecDeque},
    sync::Arc,
};

use alloy_primitives::{
    U256,
    map::{HashMap, HashSet},
};
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_transaction_pool::{
    BestTransactions, TransactionOrdering, ValidPoolTransaction, error::InvalidPoolTransactionError,
};

use crate::{BasePooledTx, BestTransactionPriority, PoolStore, two_d_nonce_pool::LaneId};

/// A store's best-transactions iterator as the merge drives it.
///
/// When the merge sets a lane's head aside, it asks the store to hold the rest
/// of that lane rather than pulling the whole lane into the merge one member
/// at a time. Iterators that cannot hold lanes keep the no-op defaults; the
/// merge then holds every member it pulls from such a lane itself.
pub(crate) trait MergeSource<T: BasePooledTx>:
    BestTransactions<Item = Arc<ValidPoolTransaction<T>>>
{
    /// Stops offering members of `lane_id` until [`Self::release_lane`].
    fn hold_lane(&mut self, _lane_id: &LaneId) {}

    /// Offers the members of a held lane again, from where it stopped.
    fn release_lane(&mut self, _lane_id: &LaneId) {}
}

impl<T: BasePooledTx> MergeSource<T>
    for Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>
{
}

type BoxedBest<T> = Box<dyn MergeSource<T>>;

/// Standard transactions a standard store holds queued directly behind a
/// validity lane's pending run, per lane and in nonce order. Their store never
/// yields them (it sees a nonce gap), so the merge releases them itself once
/// the validity run in front has been yielded.
pub(crate) type BridgedDescendants<T> = HashMap<LaneId, VecDeque<Arc<ValidPoolTransaction<T>>>>;

/// A buffered transaction with its priority, computed once when it was buffered.
struct Head<T: BasePooledTx, P: Ord + Clone> {
    transaction: Arc<ValidPoolTransaction<T>>,
    priority: BestTransactionPriority<P>,
}

/// One store's iterator together with its buffered head.
struct Source<T: BasePooledTx, P: Ord + Clone> {
    best: BoxedBest<T>,
    head: Option<Head<T, P>>,
}

/// A ready lane's front in [`HeldLanes::ready`]; the priority carries the
/// front's hash, which tells a live entry from one left behind by a pop or drop.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct ReadyHead<P: Ord + Clone> {
    priority: BestTransactionPriority<P>,
    lane_id: LaneId,
}

/// Transactions held back per lane, in nonce order, until the merge has
/// yielded the slot right before each lane's front.
///
/// Ready fronts sit in a heap by priority, so a [`MergeBestTransactions::pop_best`]
/// round looks at the best one in constant time and a pop costs a logarithm of
/// the ready lanes, however many lanes are held.
struct HeldLanes<T: BasePooledTx, P: Ord + Clone> {
    lanes: HashMap<LaneId, VecDeque<Arc<ValidPoolTransaction<T>>>>,
    ready: BinaryHeap<ReadyHead<P>>,
}

impl<T: BasePooledTx, P: Ord + Clone> Default for HeldLanes<T, P> {
    fn default() -> Self {
        Self { lanes: HashMap::default(), ready: BinaryHeap::new() }
    }
}

impl<T: BasePooledTx, P: Ord + Clone> HeldLanes<T, P> {
    const fn from_lanes(lanes: HashMap<LaneId, VecDeque<Arc<ValidPoolTransaction<T>>>>) -> Self {
        Self { lanes, ready: BinaryHeap::new() }
    }

    fn holds(&self, lane_id: &LaneId) -> bool {
        self.lanes.contains_key(lane_id)
    }

    /// Appends `transaction` to its lane; callers push a lane in nonce order.
    fn push(&mut self, lane_id: LaneId, transaction: Arc<ValidPoolTransaction<T>>) {
        self.lanes.entry(lane_id).or_default().push_back(transaction);
    }

    /// Marks the lane ready when its front directly follows the yielded `nonce`.
    fn release_after(
        &mut self,
        lane_id: LaneId,
        nonce: u64,
        priority_of: impl Fn(&Arc<ValidPoolTransaction<T>>) -> BestTransactionPriority<P>,
    ) {
        let front = self
            .lanes
            .get(&lane_id)
            .and_then(VecDeque::front)
            .filter(|front| Some(front.nonce()) == nonce.checked_add(1));
        if let Some(front) = front {
            self.ready.push(ReadyHead { priority: priority_of(front), lane_id });
        }
    }

    /// The best ready front, discarding entries whose lane was popped or dropped since.
    fn peek_ready(&mut self) -> Option<&BestTransactionPriority<P>> {
        while let Some(top) = self.ready.peek() {
            let live = self
                .lanes
                .get(&top.lane_id)
                .and_then(VecDeque::front)
                .is_some_and(|front| front.hash() == top.priority.hash());
            if live {
                break;
            }
            self.ready.pop();
        }
        self.ready.peek().map(|top| &top.priority)
    }

    /// Takes the best ready front; the next member of its lane waits for it to
    /// be yielded. Lanes left empty are removed.
    fn pop_ready(&mut self) -> Option<(LaneId, Arc<ValidPoolTransaction<T>>)> {
        self.peek_ready()?;
        let lane_id = self.ready.pop()?.lane_id;
        let lane = self.lanes.get_mut(&lane_id)?;
        let front = lane.pop_front()?;
        if lane.is_empty() {
            self.lanes.remove(&lane_id);
        }
        Some((lane_id, front))
    }

    /// Removes every held member of `lane_id`; nothing behind an invalid
    /// transaction executes.
    fn drop_lane(&mut self, lane_id: &LaneId) -> Option<VecDeque<Arc<ValidPoolTransaction<T>>>> {
        self.lanes.remove(lane_id)
    }
}

/// Where the best candidate of a [`MergeBestTransactions::pop_best`] round came from.
#[derive(Clone, Copy)]
enum Candidate {
    Source(usize),
    Deferred,
    Bridged,
}

/// Merges the best-transaction iterators of every [`PoolStore`] into one
/// priority order under a single ordering.
///
/// Each store yields a sender's transactions in nonce order, but a lane can
/// straddle stores: a validity lane may continue a run the protocol pool or
/// the sidecar holds the start of, and a standard store may hold queued
/// transactions right behind a validity run. The merge therefore releases a
/// cross-store successor only once it has yielded the slot right before it,
/// so the builder never sees nonce `n + 1` ahead of `n` however the two are
/// priced. A validity head that is not yet released is set aside in
/// `deferred`, its lane is held in the validity iterator, and that iterator
/// keeps advancing, so one sender's waiting lane never holds back the others.
///
/// Invalidation is lane-wide across stores: the store owning the transaction
/// parks the rest of its lane, any other store's buffered or held member of
/// the lane is dropped and parked there too, and nothing pulled from the lane
/// afterwards is offered.
pub(crate) struct MergeBestTransactions<T: BasePooledTx, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    sources: [Source<T, O::PriorityValue>; PoolStore::ALL.len()],
    ordering: O,
    base_fee: u64,
    /// Validity lanes riding another store's pending run, with the canonical
    /// nonce each one is anchored at. Empty unless such lanes exist.
    external_anchors: HashMap<LaneId, u64>,
    /// Validity lane members pulled from their iterator before their
    /// cross-store predecessor was yielded.
    deferred: HeldLanes<T, O::PriorityValue>,
    /// Queued standard transactions behind validity runs; see [`BridgedDescendants`].
    bridged: HeldLanes<T, O::PriorityValue>,
    /// The latest nonce yielded in each lane that straddles stores. A lane
    /// yields in nonce order, so this one slot says which successor is next;
    /// the entry goes when a member of the lane is marked invalid.
    yielded: HashMap<LaneId, u64>,
    /// Lanes with a member marked invalid; nothing else pulled from them is offered.
    invalid_lanes: HashSet<LaneId>,
}

impl<T: BasePooledTx, O> MergeBestTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    /// Creates a merged iterator over the protocol pool, sidecar and validity
    /// pool iterators, in [`PoolStore::ALL`] order.
    pub(crate) fn new(
        protocol: BoxedBest<T>,
        sidecar: BoxedBest<T>,
        validity: BoxedBest<T>,
        ordering: O,
        base_fee: u64,
    ) -> Self {
        let sources = [protocol, sidecar, validity].map(|best| Source { best, head: None });
        Self {
            sources,
            ordering,
            base_fee,
            external_anchors: HashMap::default(),
            deferred: HeldLanes::default(),
            bridged: HeldLanes::default(),
            yielded: HashMap::default(),
            invalid_lanes: HashSet::default(),
        }
    }

    /// Registers the validity lanes whose executable cursor sits past their
    /// canonical anchor (see `TwoDNoncePool::lanes_ahead_of_state`), so their
    /// heads wait for the run in front of them to be yielded first.
    pub(crate) fn with_external_anchors(mut self, anchors: HashMap<LaneId, u64>) -> Self {
        self.external_anchors = anchors;
        self
    }

    /// Registers queued standard transactions sitting right behind validity
    /// runs, to be yielded once the run in front of them has been.
    pub(crate) fn with_bridged_descendants(mut self, bridged: BridgedDescendants<T>) -> Self {
        self.bridged = HeldLanes::from_lanes(bridged);
        self
    }

    /// The nonce slot `transaction` occupies, with the protocol nonce as key
    /// zero. Nonce-free transactions have no slot.
    fn slot(transaction: &ValidPoolTransaction<T>) -> Option<(LaneId, u64)> {
        if transaction.transaction.is_eip8130_nonce_free() {
            return None;
        }
        let nonce_key = transaction.transaction.eip8130_nonce_channel_key().unwrap_or(U256::ZERO);
        Some(((transaction.sender(), nonce_key), transaction.nonce()))
    }

    fn in_lane(transaction: &ValidPoolTransaction<T>, lane_id: &LaneId) -> bool {
        Self::slot(transaction).is_some_and(|(lane, _)| lane == *lane_id)
    }

    /// The error a store is handed for a member whose lane predecessor was
    /// marked invalid: the account's nonce stays behind it. Advisory only:
    /// the stores park the lane whatever the kind, so `state` is approximate.
    const fn predecessor_invalid(nonce: u64) -> InvalidPoolTransactionError {
        InvalidPoolTransactionError::Consensus(InvalidTransactionError::NonceNotConsistent {
            tx: nonce,
            state: nonce.saturating_sub(1),
        })
    }

    fn priority(
        &self,
        transaction: &Arc<ValidPoolTransaction<T>>,
    ) -> BestTransactionPriority<O::PriorityValue> {
        BestTransactionPriority::new(&self.ordering, transaction, self.base_fee)
    }

    /// Whether a lane's bookkeeping crosses stores, i.e. yields in it must be remembered.
    fn straddles(&self, lane_id: &LaneId) -> bool {
        self.external_anchors.contains_key(lane_id) || self.bridged.holds(lane_id)
    }

    /// Whether the validity transaction may be offered as soon as its iterator
    /// yields it: either its lane is anchored at canonical state, so the lane's
    /// own order suffices, or the slot right before it has already been yielded.
    fn released(&self, lane_id: LaneId, nonce: u64) -> bool {
        let Some(anchor) = self.external_anchors.get(&lane_id) else { return true };
        nonce <= *anchor
            || nonce
                .checked_sub(1)
                .is_some_and(|predecessor| self.yielded.get(&lane_id) == Some(&predecessor))
    }

    /// Buffers the next transaction of every store, setting aside validity
    /// transactions that are not released yet and dropping members of
    /// invalidated lanes.
    fn fill_heads(&mut self) {
        for index in 0..self.sources.len() {
            while self.sources[index].head.is_none() {
                let Some(transaction) = self.sources[index].best.next() else { break };
                if let Some((lane_id, nonce)) = Self::slot(&transaction) {
                    if self.invalid_lanes.contains(&lane_id) {
                        let kind = Self::predecessor_invalid(nonce);
                        self.sources[index].best.mark_invalid(&transaction, kind);
                        continue;
                    }
                    if index == PoolStore::Validity.index() && !self.released(lane_id, nonce) {
                        self.deferred.push(lane_id, transaction);
                        self.sources[index].best.hold_lane(&lane_id);
                        continue;
                    }
                }
                let priority = self.priority(&transaction);
                self.sources[index].head = Some(Head { transaction, priority });
            }
        }
    }

    fn record_yielded(&mut self, transaction: &ValidPoolTransaction<T>) {
        let Some((lane_id, nonce)) = Self::slot(transaction) else { return };
        if self.straddles(&lane_id) {
            self.yielded.insert(lane_id, nonce);
        }
        let priority_of = |transaction: &Arc<ValidPoolTransaction<T>>| {
            BestTransactionPriority::new(&self.ordering, transaction, self.base_fee)
        };
        self.deferred.release_after(lane_id, nonce, priority_of);
        self.bridged.release_after(lane_id, nonce, priority_of);
    }

    fn pop_best(&mut self) -> Option<Arc<ValidPoolTransaction<T>>> {
        let mut best: Option<(Candidate, &BestTransactionPriority<O::PriorityValue>)> = None;
        // Ties go to the earlier candidate, so a protocol transaction wins over
        // an equally ranked sidecar, validity, deferred or bridged one.
        let mut offer = |candidate, priority| {
            if best.is_none_or(|(_, current)| priority > current) {
                best = Some((candidate, priority));
            }
        };
        for (index, source) in self.sources.iter().enumerate() {
            if let Some(head) = &source.head {
                offer(Candidate::Source(index), &head.priority);
            }
        }
        if let Some(priority) = self.deferred.peek_ready() {
            offer(Candidate::Deferred, priority);
        }
        if let Some(priority) = self.bridged.peek_ready() {
            offer(Candidate::Bridged, priority);
        }
        match best?.0 {
            Candidate::Source(index) => {
                self.sources[index].head.take().map(|head| head.transaction)
            }
            Candidate::Deferred => {
                let (lane_id, transaction) = self.deferred.pop_ready()?;
                // The validity iterator held the rest of the lane while its
                // head waited; with the head out, let it offer them again.
                if !self.deferred.holds(&lane_id) {
                    self.sources[PoolStore::Validity.index()].best.release_lane(&lane_id);
                }
                Some(transaction)
            }
            Candidate::Bridged => self.bridged.pop_ready().map(|(_, transaction)| transaction),
        }
    }
}

impl<T: BasePooledTx, O> std::fmt::Debug for MergeBestTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeBestTransactions").finish_non_exhaustive()
    }
}

impl<T: BasePooledTx, O> Iterator for MergeBestTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            self.fill_heads();

            let transaction = self.pop_best()?;
            if transaction.effective_tip_per_gas(self.base_fee).is_some() {
                self.record_yielded(&transaction);
                return Some(transaction);
            }

            self.mark_invalid(&transaction, InvalidPoolTransactionError::Underpriced);
        }
    }
}

impl<T: BasePooledTx, O> BestTransactions for MergeBestTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError) {
        let owner = PoolStore::of(&transaction.transaction).index();
        let Some((lane_id, _)) = Self::slot(transaction) else {
            // A nonce-free transaction sequences nothing; only its own store hears of it.
            self.sources[owner].best.mark_invalid(transaction, kind);
            return;
        };
        // Nothing behind a dropped transaction can execute, whichever store
        // holds it: park the lane in its own store, drop and park the lane's
        // members buffered from or held for other stores, and refuse anything
        // the lane yields from now on.
        self.invalid_lanes.insert(lane_id);
        self.yielded.remove(&lane_id);
        self.sources[owner].best.mark_invalid(transaction, kind);
        for source in &mut self.sources {
            if source.head.as_ref().is_some_and(|head| Self::in_lane(&head.transaction, &lane_id))
                && let Some(head) = source.head.take()
            {
                let kind = Self::predecessor_invalid(head.transaction.nonce());
                source.best.mark_invalid(&head.transaction, kind);
            }
        }
        if let Some(front) = self.deferred.drop_lane(&lane_id).as_ref().and_then(VecDeque::front) {
            let kind = Self::predecessor_invalid(front.nonce());
            self.sources[PoolStore::Validity.index()].best.mark_invalid(front, kind);
        }
        self.bridged.drop_lane(&lane_id);
    }

    fn no_updates(&mut self) {
        for source in &mut self.sources {
            source.best.no_updates();
        }
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        for source in &mut self.sources {
            source.best.set_skip_blobs(skip_blobs);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, time::Instant};

    use alloy_consensus::{Transaction, transaction::Recovered};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Bytes, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, Eip8130Constants, Eip8130Signed,
        TxEip8130,
    };
    use reth_transaction_pool::{TransactionOrigin, identifier::TransactionId};

    use super::*;
    use crate::{
        BaseOrdering, BasePooledTransaction, TimestampedTransaction, ValidityOperator,
        ValidityPredicate,
    };

    #[derive(Debug)]
    struct StaticBestTransactions<T: BasePooledTx> {
        transactions: VecDeque<Arc<ValidPoolTransaction<T>>>,
    }

    impl<T: BasePooledTx> StaticBestTransactions<T> {
        fn new(transactions: Vec<Arc<ValidPoolTransaction<T>>>) -> Self {
            Self { transactions: transactions.into() }
        }
    }

    impl<T: BasePooledTx> Iterator for StaticBestTransactions<T> {
        type Item = Arc<ValidPoolTransaction<T>>;

        fn next(&mut self) -> Option<Self::Item> {
            self.transactions.pop_front()
        }
    }

    impl<T: BasePooledTx> MergeSource<T> for StaticBestTransactions<T> {}

    impl<T: BasePooledTx> BestTransactions for StaticBestTransactions<T> {
        fn mark_invalid(&mut self, transaction: &Self::Item, _kind: InvalidPoolTransactionError) {
            let nonce_key = transaction.transaction.eip8130_nonce_channel_key();
            self.transactions.retain(|candidate| {
                if nonce_key.is_some() {
                    candidate.sender() != transaction.sender()
                        || candidate.transaction.eip8130_nonce_channel_key() != nonce_key
                } else {
                    candidate.sender() != transaction.sender()
                }
            });
        }

        fn no_updates(&mut self) {}

        fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
    }

    fn test_chain_id() -> u64 {
        ChainConfig::mainnet().chain_id
    }

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::random()
    }

    fn signed_tx(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        signed_tx_with_received_at(
            signer,
            nonce_key,
            nonce_sequence,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            0,
        )
    }

    fn signed_tx_with_received_at(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
        received_at: u128,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key,
            nonce_sequence,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas,
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
        let encoded_length = pooled.encode_2718_len();
        BasePooledTransaction::new_with_received_at(
            Recovered::new_unchecked(pooled.into(), signer.address()),
            encoded_length,
            received_at,
        )
    }

    fn valid_pool_transaction(
        transaction: BasePooledTransaction,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        valid_pool_transaction_at(transaction, Instant::now())
    }

    fn valid_pool_transaction_at(
        transaction: BasePooledTransaction,
        timestamp: Instant,
    ) -> Arc<ValidPoolTransaction<BasePooledTransaction>> {
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), transaction.nonce()),
            transaction,
            propagate: true,
            timestamp,
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    #[test]
    fn prefers_higher_effective_tip_across_protocol_and_sidecar() {
        let protocol = valid_pool_transaction(signed_tx(&signer(), U256::ZERO, 0, 1, 100));
        let sidecar = valid_pool_transaction(signed_tx(&signer(), U256::from(1), 0, 50, 50));
        let sidecar_hash = *sidecar.hash();

        let mut merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(vec![protocol])),
            Box::new(StaticBestTransactions::new(vec![sidecar])),
            Box::new(StaticBestTransactions::new(Vec::new())),
            BaseOrdering::coinbase_tip(),
            10,
        );

        assert_eq!(merged.next().map(|transaction| *transaction.hash()), Some(sidecar_hash));
    }

    #[test]
    fn skips_underpriced_sidecar_transactions_for_current_base_fee() {
        let sidecar = valid_pool_transaction(signed_tx(&signer(), U256::from(7), 0, 5, 5));

        let mut merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(Vec::new())),
            Box::new(StaticBestTransactions::new(vec![sidecar])),
            Box::new(StaticBestTransactions::new(Vec::new())),
            BaseOrdering::coinbase_tip(),
            10,
        );

        assert!(merged.next().is_none());
    }

    #[test]
    fn respects_timestamp_ordering_across_protocol_and_sidecar() {
        let older_protocol =
            valid_pool_transaction(signed_tx_with_received_at(&signer(), U256::ZERO, 0, 1, 100, 1));
        let newer_sidecar = valid_pool_transaction(signed_tx_with_received_at(
            &signer(),
            U256::from(9),
            0,
            50,
            50,
            2,
        ));

        let mut merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(vec![Arc::clone(&older_protocol)])),
            Box::new(StaticBestTransactions::new(vec![newer_sidecar])),
            Box::new(StaticBestTransactions::new(Vec::new())),
            BaseOrdering::timestamp(),
            10,
        );

        let first = merged.next().expect("expected a merged transaction");
        assert_eq!(first.transaction.received_at(), older_protocol.transaction.received_at());
    }

    #[test]
    fn equal_priority_prefers_earlier_submission_timestamp() {
        let now = Instant::now();
        let older_protocol =
            valid_pool_transaction_at(signed_tx(&signer(), U256::ZERO, 0, 10, 100), now);
        let newer_sidecar = valid_pool_transaction_at(
            signed_tx(&signer(), U256::from(17), 0, 10, 100),
            now + std::time::Duration::from_secs(1),
        );
        let older_hash = *older_protocol.hash();

        let mut merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(vec![older_protocol])),
            Box::new(StaticBestTransactions::new(vec![newer_sidecar])),
            Box::new(StaticBestTransactions::new(Vec::new())),
            BaseOrdering::coinbase_tip(),
            10,
        );

        assert_eq!(merged.next().map(|transaction| *transaction.hash()), Some(older_hash));
    }

    #[test]
    fn mark_invalid_clears_cached_protocol_transaction() {
        let signer = signer();
        let protocol = vec![
            valid_pool_transaction(signed_tx(&signer, U256::ZERO, 0, 1_200, 1_200)),
            valid_pool_transaction(signed_tx(&signer, U256::ZERO, 1, 900, 900)),
        ];
        let sidecar =
            vec![valid_pool_transaction(signed_tx(&signer, U256::from(1), 0, 1_000, 1_000))];

        let mut merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(protocol)),
            Box::new(StaticBestTransactions::new(sidecar)),
            Box::new(StaticBestTransactions::new(Vec::new())),
            BaseOrdering::coinbase_tip(),
            0,
        );

        let first = merged.next().expect("expected protocol transaction");
        assert_eq!(first.transaction.eip8130_nonce_channel_key(), None);
        assert_eq!(first.nonce(), 0);

        let second = merged.next().expect("expected sidecar transaction");
        assert_eq!(second.transaction.eip8130_nonce_channel_key(), Some(U256::from(1)));

        merged.mark_invalid(&first, InvalidPoolTransactionError::Underpriced);

        assert!(merged.next().is_none());
    }

    fn resting_predicates() -> Vec<ValidityPredicate> {
        vec![ValidityPredicate::BlockNumber {
            op: ValidityOperator::LessThanOrEqual,
            value: U256::from(1_000),
        }]
    }

    #[test]
    fn merges_every_store_by_priority() {
        let protocol = valid_pool_transaction(signed_tx(&signer(), U256::ZERO, 0, 100, 100));
        let sidecar = valid_pool_transaction(signed_tx(&signer(), U256::from(1), 0, 300, 300));
        let validity = valid_pool_transaction(
            signed_tx(&signer(), U256::ZERO, 0, 200, 200)
                .with_validity_predicates(resting_predicates()),
        );
        let expected = [*sidecar.hash(), *validity.hash(), *protocol.hash()];

        let merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(vec![protocol])),
            Box::new(StaticBestTransactions::new(vec![sidecar])),
            Box::new(StaticBestTransactions::new(vec![validity])),
            BaseOrdering::coinbase_tip(),
            0,
        );

        let order: Vec<_> = merged.map(|transaction| *transaction.hash()).collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn equal_priority_prefers_the_earlier_store() {
        let protocol = valid_pool_transaction(signed_tx(&signer(), U256::ZERO, 0, 100, 100));
        let sidecar = valid_pool_transaction(signed_tx(&signer(), U256::from(1), 0, 100, 100));
        let validity = valid_pool_transaction(
            signed_tx(&signer(), U256::ZERO, 0, 100, 100)
                .with_validity_predicates(resting_predicates()),
        );
        let expected = [*protocol.hash(), *sidecar.hash(), *validity.hash()];

        let merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(vec![protocol])),
            Box::new(StaticBestTransactions::new(vec![sidecar])),
            Box::new(StaticBestTransactions::new(vec![validity])),
            BaseOrdering::coinbase_tip(),
            0,
        );

        let order: Vec<_> = merged.map(|transaction| *transaction.hash()).collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn validity_successor_waits_for_the_standard_predecessor_it_outbids() {
        let signer = signer();
        let protocol = valid_pool_transaction(signed_tx(&signer, U256::ZERO, 0, 1, 1_000));
        let validity: Vec<_> = [(1, 900), (2, 800)]
            .map(|(nonce, tip)| {
                valid_pool_transaction(
                    signed_tx(&signer, U256::ZERO, nonce, tip, 1_000)
                        .with_validity_predicates(resting_predicates()),
                )
            })
            .into();
        let expected = [*protocol.hash(), *validity[0].hash(), *validity[1].hash()];
        let lane = (signer.address(), U256::ZERO);
        let build = |protocol, validity| {
            MergeBestTransactions::new(
                Box::new(StaticBestTransactions::new(protocol)),
                Box::new(StaticBestTransactions::new(Vec::new())),
                Box::new(StaticBestTransactions::new(validity)),
                BaseOrdering::coinbase_tip(),
                0,
            )
            .with_external_anchors(HashMap::from_iter([(lane, 0)]))
        };

        let merged = build(vec![Arc::clone(&protocol)], validity.clone());
        let order: Vec<_> = merged.map(|transaction| *transaction.hash()).collect();
        assert_eq!(order, expected);

        // The predecessor never shows up: the successors are never released.
        let merged = build(Vec::new(), validity.clone());
        assert_eq!(merged.count(), 0);

        // The predecessor is dropped by the builder: likewise.
        let mut merged = build(vec![protocol], validity);
        let first = merged.next().expect("protocol predecessor");
        assert_eq!(first.nonce(), 0);
        merged.mark_invalid(&first, InvalidPoolTransactionError::Underpriced);
        assert_eq!(merged.count(), 0);
    }

    #[test]
    fn a_waiting_validity_head_does_not_hold_back_other_validity_lanes() {
        let waiting = signer();
        let other = signer();
        let protocol = valid_pool_transaction(signed_tx(&waiting, U256::ZERO, 0, 1, 1_000));
        let waiting_validity = valid_pool_transaction(
            signed_tx(&waiting, U256::ZERO, 1, 900, 1_000)
                .with_validity_predicates(resting_predicates()),
        );
        let other_validity = valid_pool_transaction(
            signed_tx(&other, U256::ZERO, 0, 500, 1_000)
                .with_validity_predicates(resting_predicates()),
        );
        let build = |protocol| {
            MergeBestTransactions::new(
                Box::new(StaticBestTransactions::new(protocol)),
                Box::new(StaticBestTransactions::new(Vec::new())),
                Box::new(StaticBestTransactions::new(vec![
                    Arc::clone(&waiting_validity),
                    Arc::clone(&other_validity),
                ])),
                BaseOrdering::coinbase_tip(),
                0,
            )
            .with_external_anchors(HashMap::from_iter([((waiting.address(), U256::ZERO), 0)]))
        };

        // The waiting head is set aside; the other lane and the predecessor
        // come out by priority, then the released successor.
        let order: Vec<_> =
            build(vec![Arc::clone(&protocol)]).map(|transaction| *transaction.hash()).collect();
        assert_eq!(order, [*other_validity.hash(), *protocol.hash(), *waiting_validity.hash()]);

        // Without its predecessor the waiting head alone is left out.
        let order: Vec<_> = build(Vec::new()).map(|transaction| *transaction.hash()).collect();
        assert_eq!(order, [*other_validity.hash()]);

        // Dropping the predecessor after the fact drops the held successor too.
        let mut merged = build(vec![protocol]);
        let first = merged.next().expect("other lane");
        let second = merged.next().expect("predecessor");
        assert_eq!(second.nonce(), 0);
        merged.mark_invalid(&second, InvalidPoolTransactionError::Underpriced);
        assert_eq!(first.sender(), other.address());
        assert_eq!(merged.count(), 0);
    }

    #[test]
    fn channel_validity_successor_waits_for_the_sidecar_predecessor() {
        let signer = signer();
        let channel = U256::from(3);
        let sidecar = valid_pool_transaction(signed_tx(&signer, channel, 0, 1, 1_000));
        let validity = valid_pool_transaction(
            signed_tx(&signer, channel, 1, 900, 1_000)
                .with_validity_predicates(resting_predicates()),
        );
        let expected = [*sidecar.hash(), *validity.hash()];

        let merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(Vec::new())),
            Box::new(StaticBestTransactions::new(vec![sidecar])),
            Box::new(StaticBestTransactions::new(vec![validity])),
            BaseOrdering::coinbase_tip(),
            0,
        )
        .with_external_anchors(HashMap::from_iter([((signer.address(), channel), 0)]));

        let order: Vec<_> = merged.map(|transaction| *transaction.hash()).collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn bridged_standard_descendants_follow_the_validity_run_they_wait_for() {
        let signer = signer();
        let lane = (signer.address(), U256::ZERO);
        let validity = valid_pool_transaction(
            signed_tx(&signer, U256::ZERO, 0, 1, 1_000)
                .with_validity_predicates(resting_predicates()),
        );
        let descendants: Vec<_> = [(1, 900), (2, 950)]
            .map(|(nonce, tip)| {
                valid_pool_transaction(signed_tx(&signer, U256::ZERO, nonce, tip, 1_000))
            })
            .into();
        let expected = [*validity.hash(), *descendants[0].hash(), *descendants[1].hash()];
        let build = |validity: Vec<_>| {
            MergeBestTransactions::new(
                Box::new(StaticBestTransactions::new(Vec::new())),
                Box::new(StaticBestTransactions::new(Vec::new())),
                Box::new(StaticBestTransactions::new(validity)),
                BaseOrdering::coinbase_tip(),
                0,
            )
            .with_bridged_descendants(HashMap::from_iter([(
                lane,
                descendants.iter().cloned().collect::<VecDeque<_>>(),
            )]))
        };

        let order: Vec<_> =
            build(vec![Arc::clone(&validity)]).map(|transaction| *transaction.hash()).collect();
        assert_eq!(order, expected);

        // Without the validity head nothing bridged is released.
        assert_eq!(build(Vec::new()).count(), 0);

        // Dropping the validity head drops the bridged run with it.
        let mut merged = build(vec![validity]);
        let head = merged.next().expect("validity head");
        merged.mark_invalid(&head, InvalidPoolTransactionError::Underpriced);
        assert_eq!(merged.count(), 0);
    }

    #[test]
    fn marking_a_nonce_free_transaction_invalid_keeps_another_nonce_free_head() {
        let nonce_free = |tip| {
            valid_pool_transaction(signed_tx(
                &signer(),
                Eip8130Constants::NONCE_KEY_MAX,
                0,
                tip,
                1_000,
            ))
        };
        let (first, second) = (nonce_free(900), nonce_free(800));
        assert!(first.transaction.is_eip8130_nonce_free());
        let second_hash = *second.hash();
        let mut merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(Vec::new())),
            Box::new(StaticBestTransactions::new(vec![first, second])),
            Box::new(StaticBestTransactions::new(Vec::new())),
            BaseOrdering::coinbase_tip(),
            0,
        );
        let head = merged.next().expect("first nonce-free transaction");
        merged.mark_invalid(&head, InvalidPoolTransactionError::Underpriced);
        assert_eq!(merged.next().map(|transaction| *transaction.hash()), Some(second_hash));
    }

    #[test]
    fn invalidating_a_validity_transaction_parks_its_lane_in_every_store() {
        // Validity lane (A, 0) holds nonces 0 and 1; a static protocol source
        // offers nonce 2 of the same lane and a transaction in another lane.
        let signer_a = signer();
        let other = PrivateKeySigner::random();
        let validity = vec![
            valid_pool_transaction(
                signed_tx(&signer_a, U256::ZERO, 0, 500, 500)
                    .with_validity_predicates(resting_predicates()),
            ),
            valid_pool_transaction(
                signed_tx(&signer_a, U256::ZERO, 1, 400, 400)
                    .with_validity_predicates(resting_predicates()),
            ),
        ];
        let protocol = vec![
            valid_pool_transaction(signed_tx(&signer_a, U256::ZERO, 2, 100, 100)),
            valid_pool_transaction(signed_tx(&other, U256::ZERO, 0, 50, 50)),
        ];
        let other_hash = *protocol[1].hash();

        let mut merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(protocol)),
            Box::new(StaticBestTransactions::new(Vec::new())),
            Box::new(StaticBestTransactions::new(validity)),
            BaseOrdering::coinbase_tip(),
            0,
        );

        let head = merged.next().expect("validity head");
        assert_eq!(head.nonce(), 0);
        merged.mark_invalid(&head, InvalidPoolTransactionError::Underpriced);

        // Nothing behind nonce 0 executes, whichever store holds it; other lanes go on.
        let rest: Vec<_> = merged.map(|transaction| *transaction.hash()).collect();
        assert_eq!(rest, vec![other_hash]);
    }

    #[test]
    fn invalidating_a_standard_predecessor_drops_the_buffered_validity_successor() {
        // Protocol holds (A, 0) plus two other senders; validity holds (A, 1)
        // and (A, 2) riding (A, 0), plus an unrelated lane. The priorities are
        // arranged so (A, 1) is pulled into the validity head right after
        // (A, 0) was yielded, then loses a round to a standard transaction.
        let (a, b, c, d) = (
            signer(),
            PrivateKeySigner::random(),
            PrivateKeySigner::random(),
            PrivateKeySigner::random(),
        );
        let protocol = vec![
            valid_pool_transaction(signed_tx(&a, U256::ZERO, 0, 300, 300)),
            valid_pool_transaction(signed_tx(&b, U256::ZERO, 0, 95, 95)),
            valid_pool_transaction(signed_tx(&c, U256::ZERO, 0, 50, 50)),
        ];
        let validity = vec![
            valid_pool_transaction(
                signed_tx(&d, U256::ZERO, 0, 200, 200)
                    .with_validity_predicates(resting_predicates()),
            ),
            valid_pool_transaction(
                signed_tx(&a, U256::ZERO, 1, 90, 90).with_validity_predicates(resting_predicates()),
            ),
            valid_pool_transaction(
                signed_tx(&a, U256::ZERO, 2, 80, 80).with_validity_predicates(resting_predicates()),
            ),
        ];
        let (b0, c0, d0) = (*protocol[1].hash(), *protocol[2].hash(), *validity[0].hash());
        let anchors = HashMap::from_iter([((a.address(), U256::ZERO), 0)]);

        let mut merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(protocol)),
            Box::new(StaticBestTransactions::new(Vec::new())),
            Box::new(StaticBestTransactions::new(validity)),
            BaseOrdering::coinbase_tip(),
            0,
        )
        .with_external_anchors(anchors);

        let a0 = merged.next().expect("standard (A, 0)");
        assert_eq!((a0.sender(), a0.nonce()), (a.address(), 0));
        assert_eq!(merged.next().map(|tx| *tx.hash()), Some(d0));
        // This round pulls the released (A, 1) into the validity head, where
        // it loses to (B, 0) and stays buffered.
        assert_eq!(merged.next().map(|tx| *tx.hash()), Some(b0));

        merged.mark_invalid(&a0, InvalidPoolTransactionError::Underpriced);

        let rest: Vec<_> = merged.map(|transaction| *transaction.hash()).collect();
        assert_eq!(rest, vec![c0]);
    }

    #[test]
    fn ready_held_lanes_are_offered_by_priority() {
        // Two validity heads release two bridged standard children; the
        // children are offered against each other and the remaining sources
        // by priority, not by lane.
        let (a, b, c) = (signer(), PrivateKeySigner::random(), PrivateKeySigner::random());
        let validity = vec![
            valid_pool_transaction(
                signed_tx(&a, U256::ZERO, 0, 900, 900)
                    .with_validity_predicates(resting_predicates()),
            ),
            valid_pool_transaction(
                signed_tx(&b, U256::ZERO, 0, 800, 800)
                    .with_validity_predicates(resting_predicates()),
            ),
        ];
        let child_a = valid_pool_transaction(signed_tx(&a, U256::ZERO, 1, 10, 10));
        let child_b = valid_pool_transaction(signed_tx(&b, U256::ZERO, 1, 30, 30));
        let standard_c = valid_pool_transaction(signed_tx(&c, U256::ZERO, 0, 20, 20));
        let expected = vec![
            *validity[0].hash(),
            *validity[1].hash(),
            *child_b.hash(),
            *standard_c.hash(),
            *child_a.hash(),
        ];
        let bridged = BridgedDescendants::from_iter([
            ((a.address(), U256::ZERO), VecDeque::from([child_a])),
            ((b.address(), U256::ZERO), VecDeque::from([child_b])),
        ]);

        let merged = MergeBestTransactions::new(
            Box::new(StaticBestTransactions::new(vec![standard_c])),
            Box::new(StaticBestTransactions::new(Vec::new())),
            Box::new(StaticBestTransactions::new(validity)),
            BaseOrdering::coinbase_tip(),
            0,
        )
        .with_bridged_descendants(bridged);

        assert_eq!(merged.map(|tx| *tx.hash()).collect::<Vec<_>>(), expected);
    }
}
