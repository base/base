//! Merged best-transaction iteration across the protocol pool, the EIP-8130
//! sidecar and the validity pool.

use std::{collections::VecDeque, sync::Arc};

use alloy_primitives::{
    U256,
    map::{HashMap, HashSet},
};
use reth_transaction_pool::{
    BestTransactions, TransactionOrdering, ValidPoolTransaction, error::InvalidPoolTransactionError,
};

use crate::{BasePooledTx, BestTransactionPriority, PoolStore, two_d_nonce_pool::LaneId};

type BoxedBest<T> = Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>;

/// Standard transactions a standard store holds queued directly behind a
/// validity lane's pending run, per lane and in nonce order. Their store never
/// yields them (it sees a nonce gap), so the merge releases them itself once
/// the validity run in front has been yielded.
pub(crate) type BridgedDescendants<T> = HashMap<LaneId, VecDeque<Arc<ValidPoolTransaction<T>>>>;

/// One store's iterator together with its buffered head.
struct Source<T: BasePooledTx> {
    best: BoxedBest<T>,
    head: Option<Arc<ValidPoolTransaction<T>>>,
}

/// Where the best candidate of a [`MergeBestTransactions::pop_best`] round came from.
#[derive(Clone, Copy)]
enum Candidate {
    Source(usize),
    Bridged(LaneId),
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
/// priced.
///
/// Invalidation is routed back to the store that owns the transaction, so a
/// skipped lane head also parks the rest of its lane in every store.
pub(crate) struct MergeBestTransactions<T: BasePooledTx, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    sources: [Source<T>; PoolStore::ALL.len()],
    ordering: O,
    base_fee: u64,
    /// Validity lanes riding another store's pending run, with the canonical
    /// nonce each one is anchored at. Empty unless such lanes exist.
    external_anchors: HashMap<LaneId, u64>,
    /// Queued standard transactions behind validity runs; see [`BridgedDescendants`].
    bridged: BridgedDescendants<T>,
    /// Slots `(lane, nonce)` yielded so far in lanes that straddle stores; a
    /// slot is removed again when its transaction is marked invalid.
    yielded: HashSet<(LaneId, u64)>,
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
            bridged: HashMap::default(),
            yielded: HashSet::default(),
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
        self.bridged = bridged;
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

    /// Whether a lane's bookkeeping crosses stores, i.e. yields in it must be remembered.
    fn straddles(&self, lane_id: &LaneId) -> bool {
        self.external_anchors.contains_key(lane_id) || self.bridged.contains_key(lane_id)
    }

    fn predecessor_yielded(&self, lane_id: LaneId, nonce: u64) -> bool {
        nonce
            .checked_sub(1)
            .is_some_and(|predecessor| self.yielded.contains(&(lane_id, predecessor)))
    }

    /// Whether the validity head `transaction` may be yielded now: either its
    /// lane is anchored at canonical state, so the lane's own order suffices,
    /// or the slot right before it has already been yielded by some store.
    fn released(&self, transaction: &ValidPoolTransaction<T>) -> bool {
        let Some((lane_id, nonce)) = Self::slot(transaction) else { return true };
        let Some(anchor) = self.external_anchors.get(&lane_id) else { return true };
        nonce <= *anchor || self.predecessor_yielded(lane_id, nonce)
    }

    fn record_yielded(&mut self, transaction: &ValidPoolTransaction<T>) {
        if let Some((lane_id, nonce)) = Self::slot(transaction)
            && self.straddles(&lane_id)
        {
            self.yielded.insert((lane_id, nonce));
        }
    }

    fn pop_best(&mut self) -> Option<Arc<ValidPoolTransaction<T>>> {
        let mut best: Option<(Candidate, BestTransactionPriority<O::PriorityValue>)> = None;
        let offer =
            |candidate: Candidate,
             head: &Arc<ValidPoolTransaction<T>>,
             best: &mut Option<(Candidate, BestTransactionPriority<_>)>| {
                let priority = BestTransactionPriority::new(&self.ordering, head, self.base_fee);
                // Ties go to the earlier candidate, so a protocol transaction wins
                // over an equally ranked sidecar, validity or bridged one.
                if best.as_ref().is_none_or(|(_, current)| priority > *current) {
                    *best = Some((candidate, priority));
                }
            };
        for (index, source) in self.sources.iter().enumerate() {
            let Some(head) = &source.head else { continue };
            if index == PoolStore::Validity.index() && !self.released(head) {
                continue;
            }
            offer(Candidate::Source(index), head, &mut best);
        }
        // Bridged lanes are few: only senders with both a validity run and
        // standard transactions queued behind it.
        for (lane_id, lane) in &self.bridged {
            let Some(head) = lane.front() else { continue };
            if !self.predecessor_yielded(*lane_id, head.nonce()) {
                continue;
            }
            offer(Candidate::Bridged(*lane_id), head, &mut best);
        }
        match best?.0 {
            Candidate::Source(index) => self.sources[index].head.take(),
            Candidate::Bridged(lane_id) => self.bridged.get_mut(&lane_id)?.pop_front(),
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
            for source in &mut self.sources {
                if source.head.is_none() {
                    source.head = source.best.next();
                }
            }

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
        let source = &mut self.sources[PoolStore::of(&transaction.transaction).index()];
        source.head = None;
        source.best.mark_invalid(transaction, kind);
        // Nothing behind a dropped transaction can execute: successors in
        // other stores must not be released, and bridged ones are gone.
        if let Some((lane_id, nonce)) = Self::slot(transaction) {
            self.yielded.remove(&(lane_id, nonce));
            if let Some(lane) = self.bridged.get_mut(&lane_id) {
                lane.clear();
            }
        }
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
        BasePooledTransaction as ConsensusPooledTransaction, Eip8130Signed, TxEip8130,
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
    fn mark_invalid_routes_a_validity_transaction_to_its_own_store() {
        let validity_signer = signer();
        let validity = vec![
            valid_pool_transaction(
                signed_tx(&validity_signer, U256::ZERO, 0, 500, 500)
                    .with_validity_predicates(resting_predicates()),
            ),
            valid_pool_transaction(
                signed_tx(&validity_signer, U256::ZERO, 1, 400, 400)
                    .with_validity_predicates(resting_predicates()),
            ),
        ];
        // A standard transaction from the same sender lives in the protocol
        // store and must not be parked by the validity lane's invalidation.
        let protocol =
            vec![valid_pool_transaction(signed_tx(&validity_signer, U256::ZERO, 2, 100, 100))];
        let protocol_hash = *protocol[0].hash();

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

        let rest: Vec<_> = merged.map(|transaction| *transaction.hash()).collect();
        assert_eq!(rest, vec![protocol_hash]);
    }
}
