//! Generalized lane-aware storage for validated pool transactions.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap, HashSet},
    sync::Arc,
    time::Instant,
};

use alloy_primitives::{
    Address, B256, TxHash, U256,
    map::{B256Map, HashMap},
};
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_transaction_pool::{
    AddedTransactionOutcome, BestTransactions, PoolConfig, PoolResult, SubPool,
    TransactionOrdering, ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolError, PoolErrorKind},
    pool::{AddedTransactionState, QueuedReason},
};

use crate::{
    BasePooledTx, BaseTransactionIdentity, BaseTransactionLane, BestTransactionPriority,
    ValidatedFunding,
};

type StoredTransaction<T> = Arc<ValidPoolTransaction<T>>;
type DemotedTransaction<T> = (StoredTransaction<T>, SubPool);

/// The reason a nonce-bearing transaction is not currently executable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneGap {
    /// A nonce before this transaction is absent from the lane.
    Missing {
        /// First nonce required by the lane.
        expected: u64,
        /// First transaction nonce observed after the gap.
        found: u64,
    },
    /// An earlier transaction is present but blocked by the current base fee.
    BlockedByBaseFee {
        /// Nonce of the fee-blocked ancestor.
        ancestor: u64,
    },
    /// An earlier transaction does not currently have an execution-funding reservation.
    BlockedByFunding {
        /// Nonce of the unfunded ancestor.
        ancestor: u64,
    },
}

/// The reason a transaction does not currently hold an execution-funding reservation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FundingWaitReason {
    /// Validation did not attach funding metadata.
    MissingValidatedFunding,
    /// No current balance is known for the validated payer.
    UnknownPayerBalance {
        /// Account responsible for the transaction's maximum cost.
        payer: Address,
    },
    /// The payer's unreserved balance cannot cover this transaction.
    InsufficientPayerBalance {
        /// Account responsible for the transaction's maximum cost.
        payer: Address,
        /// Complete validated maximum cost.
        required: U256,
        /// Current balance not already reserved by incumbents.
        available: U256,
    },
    /// A missing nonce prevents this transaction from extending the reserved lane prefix.
    LaneGap {
        /// First nonce required by the reserved prefix.
        expected: u64,
        /// First transaction nonce observed after the gap.
        found: u64,
    },
    /// An earlier transaction prevents this transaction from extending the reserved lane prefix.
    BlockedByLaneFunding {
        /// Nonce of the first unfunded ancestor.
        ancestor: u64,
    },
}

/// Current execution-funding classification of a stored transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionFundingState {
    /// The validated maximum cost is reserved against the payer's known balance.
    Reserved(ValidatedFunding),
    /// The transaction is waiting for a reservation.
    Waiting(FundingWaitReason),
}

/// A transaction whose funding state changed during a store operation.
#[derive(Debug)]
pub struct FundingTransition<T: BasePooledTx> {
    /// Transaction whose funding state changed.
    pub transaction: Arc<ValidPoolTransaction<T>>,
    /// Funding state before the operation, or `None` for a newly inserted transaction.
    pub previous: Option<TransactionFundingState>,
    /// Funding state after the operation.
    pub current: TransactionFundingState,
}

/// Current executable-state classification of a stored transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneTransactionState {
    /// The transaction and all of its dependencies are executable.
    Pending,
    /// The transaction is next in its lane but its fee cap is below the base fee.
    BaseFee,
    /// The transaction is sequenced and priced for execution but lacks funding.
    Funding(FundingWaitReason),
    /// The transaction is blocked by a lane dependency.
    Queued(LaneGap),
}

/// Aggregate transaction counts and encoded sizes by state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LaneStoreSize {
    /// Number of pending transactions.
    pub pending: usize,
    /// Encoded size of pending transactions.
    pub pending_size: usize,
    /// Number of base-fee transactions.
    pub basefee: usize,
    /// Encoded size of base-fee transactions.
    pub basefee_size: usize,
    /// Number of queued transactions.
    pub queued: usize,
    /// Encoded size of queued transactions.
    pub queued_size: usize,
    /// Total number of transactions.
    pub total: usize,
}

/// Result of inserting a validated transaction.
#[derive(Debug)]
pub struct LaneInsertOutcome<T: BasePooledTx> {
    /// Reth-compatible insertion state and transaction hash.
    pub outcome: AddedTransactionOutcome,
    /// Transaction replaced at the same canonical Base identity.
    pub replaced: Option<Arc<ValidPoolTransaction<T>>>,
    /// Existing transactions newly promoted to pending by this insertion.
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Reservation changes caused by this insertion.
    pub funding_transitions: Vec<FundingTransition<T>>,
    /// Unified lifecycle transitions caused by this insertion.
    pub transitions: LaneTransitionBatch<T>,
}

/// Minimal result of a preflighted insertion without lifecycle snapshot generation.
#[derive(Debug)]
pub struct LaneRawInsertOutcome<T: BasePooledTx> {
    /// Reth-compatible added state at the end of the mutation.
    pub outcome: AddedTransactionOutcome,
    /// Replaced active transaction, if any.
    pub replaced: Option<Arc<ValidPoolTransaction<T>>>,
}

/// State changes caused by updating a lane cursor.
#[derive(Debug)]
pub struct LaneUpdateOutcome<T: BasePooledTx> {
    /// Transactions made stale and removed by the cursor advance.
    pub removed: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Transactions newly promoted to pending.
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Transactions moved out of pending.
    pub demoted: Vec<(Arc<ValidPoolTransaction<T>>, SubPool)>,
    /// Reservation changes caused by the cursor update.
    pub funding_transitions: Vec<FundingTransition<T>>,
    /// Unified lifecycle transitions caused by the cursor update.
    pub transitions: LaneTransitionBatch<T>,
}

/// State changes caused by updating the store's base fee.
#[derive(Debug)]
pub struct LaneFeeUpdateOutcome<T: BasePooledTx> {
    /// Transactions newly promoted to pending.
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Transactions moved out of pending.
    pub demoted: Vec<(Arc<ValidPoolTransaction<T>>, SubPool)>,
    /// Unified lifecycle transitions caused by the fee update.
    pub transitions: LaneTransitionBatch<T>,
}

/// Result of committing canonical Base transaction identities.
#[derive(Debug)]
pub struct LaneCommitOutcome<T: BasePooledTx> {
    /// Pool transactions removed by the commit.
    pub removed: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Remaining transactions newly promoted to pending.
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Reservation changes among transactions retained by the commit.
    pub funding_transitions: Vec<FundingTransition<T>>,
    /// Unified lifecycle transitions caused by the commit.
    pub transitions: LaneTransitionBatch<T>,
}

/// Canonical account state applied together with mined transactions and the pending base fee.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaneCanonicalAccountUpdate {
    /// Account whose protocol lane and payer balance changed.
    pub address: Address,
    /// Canonical protocol nonce after the update.
    pub nonce: u64,
    /// Canonical balance after the update.
    pub balance: U256,
}

/// Atomic result of applying one canonical pool update.
#[derive(Debug)]
pub struct LaneCanonicalUpdateOutcome<T: BasePooledTx> {
    /// Transactions removed by mining or canonical nonce advancement.
    pub removed: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Transactions that were present by hash and mined in the canonical block.
    pub mined: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Unified lifecycle transitions after one funding rebalance.
    pub transitions: LaneTransitionBatch<T>,
}

/// Result of restoring transactions from an abandoned speculative generation.
#[derive(Debug)]
pub struct LaneRestoreOutcome<T: BasePooledTx> {
    /// Transactions restored to store ownership.
    pub restored: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Transactions skipped because a newer hash already occupies their identity.
    pub skipped: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Coalesced lifecycle transitions caused by restoration.
    pub transitions: LaneTransitionBatch<T>,
}

/// State changes caused by updating or removing a payer's known balance.
#[derive(Debug)]
pub struct PayerBalanceUpdateOutcome<T: BasePooledTx> {
    /// Payer whose balance changed.
    pub payer: Address,
    /// Previously known balance.
    pub previous_balance: Option<U256>,
    /// Newly known balance, or `None` when the cached balance was removed.
    pub balance: Option<U256>,
    /// Transactions newly made executable.
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Transactions moved out of the pending subpool.
    pub demoted: Vec<(Arc<ValidPoolTransaction<T>>, SubPool)>,
    /// Reservation changes caused by the balance update.
    pub funding_transitions: Vec<FundingTransition<T>>,
    /// Unified lifecycle transitions caused by the balance update.
    pub transitions: LaneTransitionBatch<T>,
}

/// Operation that produced a lane-store lifecycle batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneTransitionCause {
    /// A validated transaction was inserted or replaced an incumbent.
    Insert,
    /// A sequential lane cursor changed.
    LaneCursor,
    /// A payer balance changed.
    PayerBalance,
    /// The base fee changed.
    BaseFee,
    /// Transactions were explicitly removed.
    Removal,
    /// Canonical identities were committed without block metadata.
    Commit,
    /// Transactions were mined in a known block.
    Mining,
    /// Transactions expired.
    Expiry,
    /// Transactions were evicted to satisfy pool limits.
    Eviction,
}

/// Terminal disposition of a transaction lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneTerminalEvent {
    /// The transaction was replaced by another hash.
    Replaced {
        /// Hash of the replacement transaction.
        by: TxHash,
    },
    /// The transaction was explicitly removed.
    Removed,
    /// The transaction became permanently invalid.
    Invalid,
    /// The transaction expired.
    Expired,
    /// The transaction was evicted to satisfy configured limits.
    Evicted,
    /// The transaction was committed without a known block hash.
    Committed,
    /// The transaction was mined in a known block.
    Mined {
        /// Hash of the block containing the transaction.
        block_hash: B256,
    },
}

/// One transaction's complete before-and-after lifecycle transition.
#[derive(Debug)]
pub struct LaneTransactionTransition<T: BasePooledTx> {
    /// Transaction affected by the operation.
    pub transaction: Arc<ValidPoolTransaction<T>>,
    /// Executable state before the operation.
    pub previous_state: Option<LaneTransactionState>,
    /// Executable state after the operation.
    pub current_state: Option<LaneTransactionState>,
    /// Funding state before the operation.
    pub previous_funding: Option<TransactionFundingState>,
    /// Funding state after the operation.
    pub current_funding: Option<TransactionFundingState>,
    /// Terminal disposition, if this operation ended the lifecycle.
    pub terminal: Option<LaneTerminalEvent>,
}

/// Coherent transaction lifecycle changes produced by one store operation.
#[derive(Debug)]
pub struct LaneTransitionBatch<T: BasePooledTx> {
    /// Operation that produced this batch.
    pub cause: LaneTransitionCause,
    /// Deterministically hash-ordered transaction transitions.
    pub transitions: Vec<LaneTransactionTransition<T>>,
}

/// Opaque lifecycle snapshot used to coalesce a multi-step store operation.
#[derive(Debug)]
pub struct LaneStoreSnapshot<T: BasePooledTx> {
    transactions: B256Map<TransitionSnapshot<T>>,
}

/// A sparse lifecycle journal for mutations affecting selected lanes and payers.
#[derive(Debug)]
pub struct LaneMutationJournal<T: BasePooledTx> {
    before: B256Map<Option<TransitionSnapshot<T>>>,
    lanes: HashSet<BaseTransactionLane>,
    payers: HashSet<Address>,
    replays: HashSet<TxHash>,
    scanned_lanes: HashSet<BaseTransactionLane>,
    scanned_payers: HashSet<Address>,
    scanned_replays: HashSet<TxHash>,
}

impl<T: BasePooledTx> Default for LaneMutationJournal<T> {
    fn default() -> Self {
        Self {
            before: B256Map::default(),
            lanes: HashSet::default(),
            payers: HashSet::default(),
            replays: HashSet::default(),
            scanned_lanes: HashSet::default(),
            scanned_payers: HashSet::default(),
            scanned_replays: HashSet::default(),
        }
    }
}

/// Terminal lifecycle record supplied when coalescing a store operation.
#[derive(Debug)]
pub struct LaneTerminalTransition<T: BasePooledTx> {
    /// Transaction whose lifecycle ended.
    pub transaction: Arc<ValidPoolTransaction<T>>,
    /// Final terminal disposition.
    pub terminal: LaneTerminalEvent,
}

impl<T: BasePooledTx> LaneTransitionBatch<T> {
    /// Returns whether this operation changed no transaction lifecycle.
    pub const fn is_empty(&self) -> bool {
        self.transitions.is_empty()
    }
}

/// Terminal reason used by explicit removal operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneRemovalReason {
    /// Explicit caller-requested removal.
    Removed,
    /// Permanent invalidation.
    Invalid,
    /// Deadline expiry.
    Expired,
    /// Capacity eviction.
    Evicted,
}

impl LaneRemovalReason {
    const fn terminal(self) -> LaneTerminalEvent {
        match self {
            Self::Removed => LaneTerminalEvent::Removed,
            Self::Invalid => LaneTerminalEvent::Invalid,
            Self::Expired => LaneTerminalEvent::Expired,
            Self::Evicted => LaneTerminalEvent::Evicted,
        }
    }

    const fn cause(self) -> LaneTransitionCause {
        match self {
            Self::Removed | Self::Invalid => LaneTransitionCause::Removal,
            Self::Expired => LaneTransitionCause::Expiry,
            Self::Evicted => LaneTransitionCause::Eviction,
        }
    }
}

/// Complete result of a removal, expiry, mining, or eviction operation.
#[derive(Debug)]
pub struct LaneRemovalOutcome<T: BasePooledTx> {
    /// Transactions removed by the operation.
    pub removed: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Remaining transactions newly promoted to pending.
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Remaining transactions moved out of pending.
    pub demoted: Vec<(Arc<ValidPoolTransaction<T>>, SubPool)>,
    /// Reservation changes among retained transactions.
    pub funding_transitions: Vec<FundingTransition<T>>,
    /// Unified lifecycle transitions caused by the operation.
    pub transitions: LaneTransitionBatch<T>,
}

#[derive(Clone, Debug)]
struct TransitionSnapshot<T: BasePooledTx> {
    transaction: Arc<ValidPoolTransaction<T>>,
    state: LaneTransactionState,
    funding: TransactionFundingState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FundingReservation {
    payer: Address,
    max_cost: U256,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FundingPriority {
    tip: u128,
    fee_cap: u128,
    timestamp: Reverse<Instant>,
    hash: TxHash,
}

#[derive(Debug)]
struct NonceLane<T: BasePooledTx> {
    cursor: u64,
    exhausted: bool,
    transactions: BTreeMap<u64, Arc<ValidPoolTransaction<T>>>,
}

impl<T: BasePooledTx> Default for NonceLane<T> {
    fn default() -> Self {
        Self { cursor: 0, exhausted: false, transactions: BTreeMap::new() }
    }
}

impl<T: BasePooledTx> NonceLane<T> {
    fn live(&self) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.transactions
            .range(self.cursor..)
            .take_while(|_| !self.exhausted)
            .map(|(_, transaction)| transaction)
    }

    fn classified(
        &self,
        base_fee: u64,
    ) -> Vec<(LaneTransactionState, &Arc<ValidPoolTransaction<T>>)> {
        let mut expected = self.cursor;
        let mut blocker = None;
        self.live()
            .map(|transaction| {
                let BaseTransactionIdentity::Nonce { nonce, .. } =
                    transaction.transaction.identity()
                else {
                    unreachable!("nonce lanes contain nonce identities")
                };
                let state = match blocker {
                    Some(gap) => LaneTransactionState::Queued(gap),
                    None if nonce != expected => {
                        let gap = LaneGap::Missing { expected, found: nonce };
                        blocker = Some(gap);
                        LaneTransactionState::Queued(gap)
                    }
                    None if transaction.transaction.max_fee_per_gas() < u128::from(base_fee) => {
                        blocker = Some(LaneGap::BlockedByBaseFee { ancestor: nonce });
                        LaneTransactionState::BaseFee
                    }
                    None => LaneTransactionState::Pending,
                };
                expected = expected.saturating_add(1);
                (state, transaction)
            })
            .collect()
    }
}

/// Pure storage and sequencing engine for protocol, finite-channel, and replay transactions.
///
/// [`BaseTransactionIdentity`] is authoritative for replacement and dependency relationships.
/// The embedded Reth `TransactionId` remains untouched for compatibility with callers, but is
/// never used as a key by this store.
#[derive(Debug)]
pub struct LaneTransactionStore<T: BasePooledTx> {
    lanes: HashMap<BaseTransactionLane, NonceLane<T>>,
    replays: B256Map<Arc<ValidPoolTransaction<T>>>,
    identities: HashMap<BaseTransactionIdentity, Arc<ValidPoolTransaction<T>>>,
    hashes: B256Map<Arc<ValidPoolTransaction<T>>>,
    senders: HashMap<Address, HashSet<TxHash>>,
    sender_ids: HashMap<reth_transaction_pool::identifier::SenderId, HashSet<TxHash>>,
    authorities: HashMap<reth_transaction_pool::identifier::SenderId, HashSet<TxHash>>,
    payer_balances: HashMap<Address, U256>,
    payer_reserved: HashMap<Address, U256>,
    payer_hashes: HashMap<Address, HashSet<TxHash>>,
    payer_reservations: HashMap<Address, HashSet<TxHash>>,
    lane_funding_heads: HashMap<BaseTransactionLane, TxHash>,
    replay_funding_candidates: HashSet<TxHash>,
    payer_funding_candidates: HashMap<Address, HashSet<TxHash>>,
    funding_candidate_payers: B256Map<Address>,
    funding_reservations: B256Map<FundingReservation>,
    config: PoolConfig,
    base_fee: u64,
    #[cfg(test)]
    full_classification_passes: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    incremental_lane_scans: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    incremental_payer_scans: std::sync::atomic::AtomicUsize,
}

impl<T: BasePooledTx> LaneTransactionStore<T> {
    /// Resets the test-only counter for whole-store classification passes.
    #[cfg(test)]
    pub fn reset_full_classification_passes(&self) {
        self.full_classification_passes.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Returns the test-only count of whole-store classification passes.
    #[cfg(test)]
    pub fn full_classification_passes(&self) -> usize {
        self.full_classification_passes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Resets test-only incremental dependency scan counters.
    #[cfg(test)]
    pub fn reset_incremental_scan_counts(&self) {
        self.incremental_lane_scans.store(0, std::sync::atomic::Ordering::Relaxed);
        self.incremental_payer_scans.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Returns test-only incremental lane and payer scan counts.
    #[cfg(test)]
    pub fn incremental_scan_counts(&self) -> (usize, usize) {
        (
            self.incremental_lane_scans.load(std::sync::atomic::Ordering::Relaxed),
            self.incremental_payer_scans.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Creates an empty store using Reth pool limits and replacement policy.
    pub fn new(config: PoolConfig, base_fee: u64) -> Self {
        Self {
            lanes: HashMap::default(),
            replays: B256Map::default(),
            identities: HashMap::default(),
            hashes: B256Map::default(),
            senders: HashMap::default(),
            sender_ids: HashMap::default(),
            authorities: HashMap::default(),
            payer_balances: HashMap::default(),
            payer_reserved: HashMap::default(),
            payer_hashes: HashMap::default(),
            payer_reservations: HashMap::default(),
            lane_funding_heads: HashMap::default(),
            replay_funding_candidates: HashSet::default(),
            payer_funding_candidates: HashMap::default(),
            funding_candidate_payers: B256Map::default(),
            funding_reservations: B256Map::default(),
            config,
            base_fee,
            #[cfg(test)]
            full_classification_passes: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            incremental_lane_scans: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            incremental_payer_scans: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Returns the current base fee used for classification.
    pub const fn base_fee(&self) -> u64 {
        self.base_fee
    }

    /// Returns the number of stored transactions.
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    /// Returns whether the store has no transactions.
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Returns whether a transaction hash is stored.
    pub fn contains_hash(&self, hash: &TxHash) -> bool {
        self.hashes.contains_key(hash)
    }

    /// Returns whether a canonical Base identity is stored.
    pub fn contains_identity(&self, identity: &BaseTransactionIdentity) -> bool {
        self.identities.contains_key(identity)
    }

    /// Returns a transaction by hash.
    pub fn get_by_hash(&self, hash: &TxHash) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.hashes.get(hash).cloned()
    }

    /// Returns a transaction by canonical Base identity.
    pub fn get_by_identity(
        &self,
        identity: &BaseTransactionIdentity,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.identities.get(identity).cloned()
    }

    /// Returns the cursor for a sequential lane.
    pub fn lane_cursor(&self, lane: &BaseTransactionLane) -> Option<u64> {
        self.lanes.get(lane).map(|lane| lane.cursor)
    }

    /// Returns the currently known balance for a payer.
    pub fn payer_balance(&self, payer: Address) -> Option<U256> {
        self.payer_balances.get(&payer).copied()
    }

    /// Returns the amount currently reserved against a payer's known balance.
    pub fn payer_reserved(&self, payer: Address) -> U256 {
        self.payer_reserved.get(&payer).copied().unwrap_or_default()
    }

    /// Returns the execution-funding classification for a stored identity.
    pub fn funding_state(
        &self,
        identity: &BaseTransactionIdentity,
    ) -> Option<TransactionFundingState> {
        let transaction = self.identities.get(identity)?;
        Some(self.funding_state_for(transaction))
    }

    /// Returns all stored transactions in deterministic hash order.
    pub fn all_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions: Vec<_> = self.hashes.values().cloned().collect();
        transactions.sort_unstable_by_key(|transaction| *transaction.hash());
        transactions
    }

    /// Returns all transactions belonging to a physical sender.
    pub fn transactions_by_sender(&self, sender: Address) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions: Vec<_> = self
            .senders
            .get(&sender)
            .into_iter()
            .flatten()
            .filter_map(|hash| self.hashes.get(hash).cloned())
            .collect();
        transactions.sort_unstable_by_key(|transaction| *transaction.hash());
        transactions
    }

    /// Returns a transaction from the sender's protocol nonce lane.
    pub fn protocol_transaction(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.get_by_identity(&BaseTransactionIdentity::Nonce {
            lane: BaseTransactionLane::Protocol { sender },
            nonce,
        })
    }

    /// Returns the highest transaction in the sender's protocol nonce lane.
    pub fn highest_protocol_transaction(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.lanes.get(&BaseTransactionLane::Protocol { sender })?.live().last().cloned()
    }

    /// Returns pending transactions in the sender's protocol nonce lane only.
    pub fn pending_protocol_transactions(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.lanes
            .get(&BaseTransactionLane::Protocol { sender })
            .into_iter()
            .flat_map(|lane| self.classified_lane(lane))
            .take_while(|(state, _)| *state == LaneTransactionState::Pending)
            .map(|(_, transaction)| Arc::clone(transaction))
            .collect()
    }

    /// Returns the active transaction count for a Reth sender identifier.
    pub fn transaction_count_by_sender_id(
        &self,
        sender_id: reth_transaction_pool::identifier::SenderId,
    ) -> usize {
        self.sender_ids.get(&sender_id).map_or(0, HashSet::len)
    }

    /// Returns whether an active transaction reserves the sender as an authority.
    pub fn has_authority(&self, sender_id: reth_transaction_pool::identifier::SenderId) -> bool {
        self.authorities.get(&sender_id).is_some_and(|hashes| !hashes.is_empty())
    }

    /// Returns the highest gapless protocol transaction starting at `on_chain_nonce`.
    pub fn highest_consecutive_protocol_transaction(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        let lane = self.lanes.get(&BaseTransactionLane::Protocol { sender })?;
        let mut expected = lane.cursor.max(on_chain_nonce);
        let mut highest = None;
        for (&nonce, transaction) in lane.transactions.range(expected..) {
            if nonce != expected {
                break;
            }
            highest = Some(Arc::clone(transaction));
            expected = expected.saturating_add(1);
        }
        highest
    }

    /// Returns replay transaction hashes whose validity deadline has elapsed.
    pub fn expired_replay_hashes(&self, now_millis: u64) -> Vec<TxHash> {
        self.replays
            .values()
            .filter_map(|transaction| {
                let signed = transaction.transaction.as_eip8130()?;
                (signed.tx().valid_before <= now_millis).then_some(*transaction.hash())
            })
            .collect()
    }

    /// Returns pending transactions belonging to a physical sender.
    pub fn pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.transactions_by_sender(sender)
            .into_iter()
            .filter(|transaction| {
                self.state(&transaction.transaction.identity())
                    == Some(LaneTransactionState::Pending)
            })
            .collect()
    }

    /// Returns queued or base-fee transactions belonging to a physical sender.
    pub fn queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.transactions_by_sender(sender)
            .into_iter()
            .filter(|transaction| {
                self.state(&transaction.transaction.identity())
                    .is_some_and(|state| state != LaneTransactionState::Pending)
            })
            .collect()
    }

    /// Returns all physical senders represented in the store.
    pub fn unique_senders(&self) -> HashSet<Address> {
        self.senders.keys().copied().collect()
    }

    /// Returns the current classification for an identity.
    pub fn state(&self, identity: &BaseTransactionIdentity) -> Option<LaneTransactionState> {
        match *identity {
            BaseTransactionIdentity::Nonce { lane, .. } => self
                .classified_lane(self.lanes.get(&lane)?)
                .into_iter()
                .find_map(|(state, transaction)| {
                    (transaction.transaction.identity() == *identity).then_some(state)
                }),
            BaseTransactionIdentity::Replay { replay_id } => self
                .replays
                .get(&replay_id)
                .and_then(|transaction| self.transition_snapshot_for_hash(transaction.hash()))
                .map(|snapshot| snapshot.state),
        }
    }

    /// Captures lifecycle state for a later coalesced transition batch.
    pub fn snapshot(&self) -> LaneStoreSnapshot<T> {
        LaneStoreSnapshot { transactions: self.transition_snapshot() }
    }

    /// Starts an empty sparse mutation journal.
    pub fn mutation_journal(&self) -> LaneMutationJournal<T> {
        LaneMutationJournal::default()
    }

    /// Captures the lanes and payer groups that can change during insertion.
    pub fn journal_insertion(
        &self,
        journal: &mut LaneMutationJournal<T>,
        transaction: &ValidPoolTransaction<T>,
    ) {
        let hash = *transaction.hash();
        let identity = transaction.transaction.identity();
        self.journal_hash(journal, hash);
        self.seed_journal_identity(journal, identity);
        if identity.is_replay() {
            journal.replays.insert(hash);
        }
        if let Some(funding) = transaction.transaction.validated_funding() {
            journal.payers.insert(funding.payer());
        }
        if let Some(existing) = self.get_by_identity(&transaction.transaction.identity())
            && let Some(funding) = existing.transaction.validated_funding()
        {
            journal.payers.insert(funding.payer());
        }
        self.expand_funding_closure(journal);
    }

    /// Captures the transitive dependency closure affected by a payer balance change.
    pub fn journal_payer_change(&self, journal: &mut LaneMutationJournal<T>, payer: Address) {
        journal.payers.insert(payer);
        self.expand_funding_closure(journal);
    }

    /// Rebalances only the dependency closure captured in `journal`.
    pub fn rebalance_funding_journaled(&mut self, journal: &LaneMutationJournal<T>) {
        self.rebalance_funding_incremental(journal);
    }

    /// Produces one coalesced transition batch for hashes captured by a sparse journal.
    pub fn transitions_since_journal(
        &self,
        journal: LaneMutationJournal<T>,
        cause: LaneTransitionCause,
        terminals: Vec<LaneTerminalTransition<T>>,
    ) -> LaneTransitionBatch<T> {
        let terminal_transactions = terminals
            .iter()
            .map(|transition| (*transition.transaction.hash(), Arc::clone(&transition.transaction)))
            .collect::<B256Map<_>>();
        let terminals = terminals
            .into_iter()
            .map(|transition| (*transition.transaction.hash(), transition.terminal))
            .collect::<B256Map<_>>();
        let mut hashes = journal.before.keys().copied().collect::<HashSet<_>>();
        hashes.extend(terminals.keys().copied());
        let mut transitions = hashes
            .into_iter()
            .filter_map(|hash| {
                let previous = journal.before.get(&hash).and_then(Option::as_ref);
                let current = self.transition_snapshot_for_hash(&hash);
                let terminal = terminals.get(&hash).copied();
                let changed = previous.map(|snapshot| snapshot.state)
                    != current.as_ref().map(|snapshot| snapshot.state)
                    || previous.map(|snapshot| snapshot.funding)
                        != current.as_ref().map(|snapshot| snapshot.funding)
                    || terminal.is_some();
                changed.then(|| LaneTransactionTransition {
                    transaction: Arc::clone(
                        current
                            .as_ref()
                            .map(|snapshot| &snapshot.transaction)
                            .or_else(|| previous.map(|snapshot| &snapshot.transaction))
                            .or_else(|| terminal_transactions.get(&hash))
                            .expect("journal transition has a transaction"),
                    ),
                    previous_state: previous.map(|snapshot| snapshot.state),
                    current_state: current.as_ref().map(|snapshot| snapshot.state),
                    previous_funding: previous.map(|snapshot| snapshot.funding),
                    current_funding: current.as_ref().map(|snapshot| snapshot.funding),
                    terminal,
                })
            })
            .collect::<Vec<_>>();
        transitions.sort_unstable_by_key(|transition| *transition.transaction.hash());
        LaneTransitionBatch { cause, transitions }
    }

    /// Applies the sparse journal's current classifications to a cached aggregate size.
    pub fn size_after_journal(
        &self,
        mut size: LaneStoreSize,
        journal: &LaneMutationJournal<T>,
    ) -> LaneStoreSize {
        for (hash, previous) in &journal.before {
            if let Some(previous) = previous {
                Self::remove_size_entry(&mut size, previous.state, &previous.transaction);
            }
            if let Some(current) = self.transition_snapshot_for_hash(hash) {
                Self::add_size_entry(&mut size, current.state, &current.transaction);
            }
        }
        size
    }

    /// Produces one transition batch from an earlier snapshot to the current store state.
    pub fn transitions_since(
        &self,
        snapshot: LaneStoreSnapshot<T>,
        cause: LaneTransitionCause,
        terminals: Vec<LaneTerminalTransition<T>>,
    ) -> LaneTransitionBatch<T> {
        let terminal_transactions = terminals
            .iter()
            .map(|transition| (*transition.transaction.hash(), Arc::clone(&transition.transaction)))
            .collect::<B256Map<_>>();
        let terminals = terminals
            .into_iter()
            .map(|transition| (*transition.transaction.hash(), transition.terminal))
            .collect();
        self.transition_batch_with_transactions(
            cause,
            &snapshot.transactions,
            terminals,
            &terminal_transactions,
        )
    }

    /// Returns aggregate counts and encoded sizes for each state.
    pub fn size(&self) -> LaneStoreSize {
        let mut size = LaneStoreSize::default();
        for (state, transaction) in self.classified_transactions() {
            size.total += 1;
            match state {
                LaneTransactionState::Pending => {
                    size.pending += 1;
                    size.pending_size += transaction.transaction.size();
                }
                LaneTransactionState::BaseFee => {
                    size.basefee += 1;
                    size.basefee_size += transaction.transaction.size();
                }
                LaneTransactionState::Funding(_) | LaneTransactionState::Queued(_) => {
                    size.queued += 1;
                    size.queued_size += transaction.transaction.size();
                }
            }
        }
        size
    }

    /// Returns all transactions currently classified as pending.
    pub fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.transactions_with_state(LaneTransactionState::Pending)
    }

    /// Returns all transactions currently parked by the base fee.
    pub fn basefee_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.transactions_with_state(LaneTransactionState::BaseFee)
    }

    /// Returns all transactions currently queued behind a lane dependency.
    pub fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions = self
            .classified_transactions()
            .filter(|(state, _)| {
                matches!(state, LaneTransactionState::Funding(_) | LaneTransactionState::Queued(_))
            })
            .map(|(_, transaction)| Arc::clone(transaction))
            .collect::<Vec<_>>();
        transactions.sort_unstable_by_key(|transaction| *transaction.hash());
        transactions
    }

    /// Inserts a validated transaction, initializing a new nonce lane at `lane_cursor`.
    ///
    /// The cursor is ignored for replay identities and does not overwrite an existing lane cursor.
    pub fn insert_validated(
        &mut self,
        transaction: ValidPoolTransaction<T>,
        lane_cursor: u64,
    ) -> PoolResult<LaneInsertOutcome<T>> {
        self.preflight_insert(&transaction, lane_cursor)?;
        Ok(self.insert_preflighted(transaction, lane_cursor))
    }

    /// Inserts a transaction after the caller has completed an equivalent preflight.
    pub fn insert_preflighted(
        &mut self,
        transaction: ValidPoolTransaction<T>,
        lane_cursor: u64,
    ) -> LaneInsertOutcome<T> {
        let hash = *transaction.hash();
        let before = self.transition_snapshot();
        let raw = self.insert_preflighted_raw(transaction, lane_cursor);
        let replaced = raw.replaced;
        let identity = self
            .hashes
            .get(&hash)
            .expect("raw insertion indexed transaction")
            .transaction
            .identity();
        let state = self.state(&identity).expect("inserted transaction is classified");
        let terminals = replaced
            .as_ref()
            .map(|transaction| {
                B256Map::from_iter([(
                    *transaction.hash(),
                    LaneTerminalEvent::Replaced { by: hash },
                )])
            })
            .unwrap_or_default();
        let transitions = self.transition_batch(LaneTransitionCause::Insert, &before, terminals);
        let promoted = transitions
            .transitions
            .iter()
            .filter(|transition| {
                *transition.transaction.hash() != hash
                    && transition.current_state == Some(LaneTransactionState::Pending)
                    && transition.previous_state != Some(LaneTransactionState::Pending)
            })
            .map(|transition| Arc::clone(&transition.transaction))
            .collect();
        let funding_transitions = Self::funding_transitions_from_batch(&transitions);
        LaneInsertOutcome {
            outcome: AddedTransactionOutcome { hash, state: Self::added_state(state) },
            replaced,
            promoted,
            funding_transitions,
            transitions,
        }
    }

    /// Performs an infallible insertion after equivalent caller-side preflight.
    pub fn insert_preflighted_raw(
        &mut self,
        transaction: ValidPoolTransaction<T>,
        lane_cursor: u64,
    ) -> LaneRawInsertOutcome<T> {
        let mut journal = self.mutation_journal();
        self.journal_insertion(&mut journal, &transaction);
        self.insert_preflighted_with_rebalance(transaction, lane_cursor, &journal)
    }

    /// Performs a preflighted insertion using only the journaled funding closure.
    pub fn insert_preflighted_journaled(
        &mut self,
        transaction: ValidPoolTransaction<T>,
        lane_cursor: u64,
        journal: &LaneMutationJournal<T>,
    ) -> LaneRawInsertOutcome<T> {
        self.insert_preflighted_with_rebalance(transaction, lane_cursor, journal)
    }

    fn insert_preflighted_with_rebalance(
        &mut self,
        transaction: ValidPoolTransaction<T>,
        lane_cursor: u64,
        journal: &LaneMutationJournal<T>,
    ) -> LaneRawInsertOutcome<T> {
        let hash = *transaction.hash();
        let identity = transaction.transaction.identity();
        let transaction = Arc::new(transaction);
        let replaced = self.identities.get(&identity).cloned();
        if let Some(existing) = &replaced {
            self.remove_indexed(existing.transaction.identity());
        }
        match identity {
            BaseTransactionIdentity::Nonce { lane, nonce } => {
                self.lanes
                    .entry(lane)
                    .or_insert_with(|| NonceLane {
                        cursor: lane_cursor,
                        exhausted: false,
                        transactions: BTreeMap::new(),
                    })
                    .transactions
                    .insert(nonce, Arc::clone(&transaction));
            }
            BaseTransactionIdentity::Replay { replay_id } => {
                self.replays.insert(replay_id, Arc::clone(&transaction));
            }
        }
        self.index(identity, Arc::clone(&transaction));
        if replaced.is_some() && self.is_funding_head(identity) {
            self.try_reserve(&transaction);
        }
        self.rebalance_funding_incremental(journal);
        let state = self.state(&identity).expect("inserted transaction is classified");
        LaneRawInsertOutcome {
            outcome: AddedTransactionOutcome { hash, state: Self::added_state(state) },
            replaced,
        }
    }

    /// Checks every fallible store-level insertion rule without mutating the store.
    pub fn preflight_insert(
        &self,
        transaction: &ValidPoolTransaction<T>,
        lane_cursor: u64,
    ) -> PoolResult<()> {
        self.preflight_insert_with_replaced(transaction, lane_cursor, None)
    }

    /// Preflights insertion against an incumbent held outside the active store.
    pub fn preflight_insert_with_replaced(
        &self,
        transaction: &ValidPoolTransaction<T>,
        lane_cursor: u64,
        external_replaced: Option<&Arc<ValidPoolTransaction<T>>>,
    ) -> PoolResult<()> {
        self.preflight_insert_with_context(transaction, lane_cursor, external_replaced, 0)
    }

    /// Preflights insertion while accounting for sender slots held outside active storage.
    pub fn preflight_insert_with_context(
        &self,
        transaction: &ValidPoolTransaction<T>,
        lane_cursor: u64,
        external_replaced: Option<&Arc<ValidPoolTransaction<T>>>,
        additional_sender_slots: usize,
    ) -> PoolResult<()> {
        let hash = *transaction.hash();
        if self.hashes.contains_key(&hash) {
            return Err(PoolError::new(hash, PoolErrorKind::AlreadyImported));
        }
        if transaction.transaction.max_fee_per_gas()
            < u128::from(self.config.minimal_protocol_basefee)
        {
            return Err(PoolError::new(
                hash,
                PoolErrorKind::FeeCapBelowMinimumProtocolFeeCap(
                    transaction.transaction.max_fee_per_gas(),
                ),
            ));
        }
        let identity = transaction.transaction.identity();
        if let BaseTransactionIdentity::Nonce { lane, nonce } = identity {
            let stored = self.lanes.get(&lane);
            let current_cursor = stored.map_or(lane_cursor, |stored| stored.cursor);
            if stored.is_some_and(|stored| stored.exhausted) || nonce < current_cursor {
                return Err(PoolError::new(
                    hash,
                    PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::Consensus(
                        InvalidTransactionError::NonceNotConsistent {
                            tx: nonce,
                            state: current_cursor,
                        },
                    )),
                ));
            }
        }
        let replaced = self.identities.get(&identity).or(external_replaced);
        if let Some(existing) = replaced
            && existing.is_underpriced(transaction, &self.config.price_bumps)
        {
            return Err(PoolError::new(hash, PoolErrorKind::ReplacementUnderpriced));
        }
        self.ensure_sender_capacity(
            transaction,
            lane_cursor,
            replaced.is_some(),
            additional_sender_slots,
        )
    }

    /// Sets a lane cursor, removing transactions made stale by a forward move.
    pub fn set_lane_cursor(
        &mut self,
        lane: BaseTransactionLane,
        cursor: u64,
    ) -> LaneUpdateOutcome<T> {
        let before = self.transition_snapshot();
        let mut journal = self.mutation_journal();
        journal.lanes.insert(lane);
        self.expand_funding_closure(&mut journal);
        let stale = {
            let lane = self.lanes.entry(lane).or_default();
            lane.cursor = cursor;
            lane.exhausted = false;
            lane.transactions
                .range(..cursor)
                .map(|(_, transaction)| *transaction.hash())
                .collect::<Vec<_>>()
        };
        let removed = self.remove_hashes_raw(&stale);
        self.rebalance_funding_incremental(&journal);
        let terminals = Self::terminals_for(&removed, LaneTerminalEvent::Committed);
        let transitions =
            self.transition_batch(LaneTransitionCause::LaneCursor, &before, terminals);
        let (promoted, demoted) = Self::pending_changes_from_batch(&transitions);
        let funding_transitions = Self::funding_transitions_from_batch(&transitions);
        LaneUpdateOutcome { removed, promoted, demoted, funding_transitions, transitions }
    }

    /// Updates the base fee and reports transactions entering or leaving pending.
    pub fn set_base_fee(&mut self, base_fee: u64) -> LaneFeeUpdateOutcome<T> {
        let previous_base_fee = self.base_fee;
        let before = self.transition_snapshot();
        self.base_fee = base_fee;
        let transitions =
            self.transition_batch(LaneTransitionCause::BaseFee, &before, B256Map::default());
        let (promoted, demoted) = Self::pending_changes_from_batch(&transitions);
        debug_assert!(previous_base_fee != base_fee || transitions.is_empty());
        LaneFeeUpdateOutcome { promoted, demoted, transitions }
    }

    /// Sets a payer's known balance and reconciles reservations deterministically.
    pub fn set_payer_balance(
        &mut self,
        payer: Address,
        balance: U256,
    ) -> PayerBalanceUpdateOutcome<T> {
        self.update_payer_balance(payer, Some(balance))
    }

    /// Sets a payer balance without generating a whole-store lifecycle snapshot.
    pub fn set_payer_balance_raw(&mut self, payer: Address, balance: U256) {
        let mut journal = self.mutation_journal();
        self.journal_payer_change(&mut journal, payer);
        self.payer_balances.insert(payer, balance);
        self.rebalance_funding_incremental(&journal);
    }

    /// Sets a payer balance before a journaled incremental rebalance.
    pub fn set_payer_balance_unbalanced(&mut self, payer: Address, balance: U256) {
        self.payer_balances.insert(payer, balance);
    }

    /// Removes a payer's known balance and releases reservations backed by it.
    pub fn remove_payer_balance(&mut self, payer: Address) -> PayerBalanceUpdateOutcome<T> {
        self.update_payer_balance(payer, None)
    }

    /// Removes expired hashes and returns one expiry lifecycle batch.
    pub fn remove_expired(&mut self, hashes: &[TxHash]) -> LaneRemovalOutcome<T> {
        self.remove_exact_with_outcome(hashes, LaneRemovalReason::Expired)
    }

    /// Removes exact hashes without advancing lane cursors.
    pub fn remove_exact(&mut self, hashes: &[TxHash]) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.remove_exact_with_outcome(hashes, LaneRemovalReason::Removed).removed
    }

    /// Removes exact hashes and returns their complete lifecycle transition batch.
    pub fn remove_exact_with_outcome(
        &mut self,
        hashes: &[TxHash],
        reason: LaneRemovalReason,
    ) -> LaneRemovalOutcome<T> {
        let before = self.transition_snapshot();
        let mut journal = self.mutation_journal();
        for hash in hashes {
            self.journal_removal(&mut journal, *hash);
        }
        let removed = self.remove_hashes_raw(hashes);
        self.rebalance_funding_incremental(&journal);
        self.removal_outcome(before, removed, reason)
    }

    /// Removes exact hashes and every nonce descendant in the same lane.
    ///
    /// Replay entries have no descendants and are removed independently.
    pub fn remove_with_descendants(
        &mut self,
        hashes: &[TxHash],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.remove_with_descendants_outcome(hashes, LaneRemovalReason::Removed).removed
    }

    /// Removes exact hashes and nonce descendants with a complete lifecycle batch.
    pub fn remove_with_descendants_outcome(
        &mut self,
        hashes: &[TxHash],
        reason: LaneRemovalReason,
    ) -> LaneRemovalOutcome<T> {
        let before = self.transition_snapshot();
        let mut journal = self.mutation_journal();
        for hash in hashes {
            self.journal_removal(&mut journal, *hash);
        }
        let mut identities = Vec::new();
        for hash in hashes {
            let Some(transaction) = self.hashes.get(hash) else { continue };
            match transaction.transaction.identity() {
                BaseTransactionIdentity::Replay { replay_id } => {
                    identities.push(BaseTransactionIdentity::Replay { replay_id });
                }
                BaseTransactionIdentity::Nonce { lane, nonce } => {
                    if let Some(stored) = self.lanes.get(&lane) {
                        identities.extend(stored.transactions.range(nonce..).map(|(nonce, _)| {
                            BaseTransactionIdentity::Nonce { lane, nonce: *nonce }
                        }));
                    }
                }
            }
        }
        identities.sort_unstable_by_key(|identity| Self::identity_sort_key(*identity));
        identities.dedup();
        let removed =
            identities.into_iter().filter_map(|identity| self.remove_indexed(identity)).collect();
        self.rebalance_funding_incremental(&journal);
        self.removal_outcome(before, removed, reason)
    }

    /// Prunes known mined hashes while preserving executable descendants.
    ///
    /// A sequential cursor advances only when the pruned transaction is its current head.
    pub fn prune(&mut self, hashes: &[TxHash]) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.prune_with_outcome(hashes, LaneRemovalReason::Removed).removed
    }

    /// Prunes hashes while preserving descendants and returns a complete lifecycle batch.
    pub fn prune_with_outcome(
        &mut self,
        hashes: &[TxHash],
        reason: LaneRemovalReason,
    ) -> LaneRemovalOutcome<T> {
        let before = self.transition_snapshot();
        let mut journal = self.mutation_journal();
        for hash in hashes {
            self.journal_removal(&mut journal, *hash);
        }
        let removed = self.prune_raw(hashes);
        self.rebalance_funding_incremental(&journal);
        self.removal_outcome(before, removed, reason)
    }

    /// Speculatively prunes hashes while preserving descendants and hash listeners.
    ///
    /// This removes ownership from the store but deliberately emits no terminal disposition;
    /// canonical mining later supplies the final event.
    pub fn prune_speculative(&mut self, hashes: &[TxHash]) -> LaneRemovalOutcome<T> {
        let before = self.transition_snapshot();
        let mut journal = self.mutation_journal();
        for hash in hashes {
            self.journal_removal(&mut journal, *hash);
        }
        let removed = self.prune_raw(hashes);
        self.rebalance_funding_incremental(&journal);
        self.removal_outcome_with_terminals(
            before,
            removed,
            LaneTransitionCause::Commit,
            B256Map::default(),
        )
    }

    /// Restores transactions removed by an abandoned speculative generation.
    pub fn restore_speculative(
        &mut self,
        mut transactions: Vec<Arc<ValidPoolTransaction<T>>>,
    ) -> LaneRestoreOutcome<T> {
        let before = self.transition_snapshot();
        let mut journal = self.mutation_journal();
        for transaction in &transactions {
            self.journal_insertion(&mut journal, transaction);
        }
        transactions.sort_unstable_by_key(|transaction| {
            Self::identity_sort_key(transaction.transaction.identity())
        });
        let mut restored = Vec::new();
        let mut skipped = Vec::new();
        for transaction in transactions {
            let identity = transaction.transaction.identity();
            if self.hashes.contains_key(transaction.hash())
                || self.identities.contains_key(&identity)
            {
                skipped.push(transaction);
                continue;
            }
            if let BaseTransactionIdentity::Nonce { lane, nonce } = identity {
                let stored = self.lanes.entry(lane).or_default();
                stored.cursor = stored.cursor.min(nonce);
                stored.exhausted = false;
                stored.transactions.insert(nonce, Arc::clone(&transaction));
            } else if let BaseTransactionIdentity::Replay { replay_id } = identity {
                self.replays.insert(replay_id, Arc::clone(&transaction));
            }
            self.index(identity, Arc::clone(&transaction));
            restored.push(transaction);
        }
        self.rebalance_funding_incremental(&journal);
        let transitions =
            self.transition_batch(LaneTransitionCause::Insert, &before, B256Map::default());
        LaneRestoreOutcome { restored, skipped, transitions }
    }

    /// Prunes transactions mined in `block_hash` and preserves executable descendants.
    pub fn prune_mined(&mut self, hashes: &[TxHash], block_hash: B256) -> LaneRemovalOutcome<T> {
        let before = self.transition_snapshot();
        let mut journal = self.mutation_journal();
        for hash in hashes {
            self.journal_removal(&mut journal, *hash);
        }
        let removed = self.prune_raw(hashes);
        self.rebalance_funding_incremental(&journal);
        let terminals = Self::terminals_for(&removed, LaneTerminalEvent::Mined { block_hash });
        self.removal_outcome_with_terminals(before, removed, LaneTransitionCause::Mining, terminals)
    }

    /// Atomically applies pending base fee, mined hashes, protocol cursors, and payer balances.
    pub fn apply_canonical_update(
        &mut self,
        base_fee: u64,
        mined_hashes: &[TxHash],
        block_hash: B256,
        accounts: &[LaneCanonicalAccountUpdate],
        invalidated_hashes: &[TxHash],
        expired_hashes: &[TxHash],
    ) -> LaneCanonicalUpdateOutcome<T> {
        let before = self.transition_snapshot();
        let mut journal = self.mutation_journal();
        for hash in mined_hashes.iter().chain(invalidated_hashes).chain(expired_hashes) {
            self.journal_removal(&mut journal, *hash);
        }
        for account in accounts {
            journal.lanes.insert(BaseTransactionLane::Protocol { sender: account.address });
            journal.payers.insert(account.address);
        }
        self.expand_funding_closure(&mut journal);
        self.base_fee = base_fee;
        let mined = self.prune_raw(mined_hashes);
        let mined_hashes =
            mined.iter().map(|transaction| *transaction.hash()).collect::<HashSet<_>>();
        let mut removed = mined.clone();
        let invalidated = self.remove_hashes_raw(invalidated_hashes);
        let invalidated_hashes =
            invalidated.iter().map(|transaction| *transaction.hash()).collect::<HashSet<_>>();
        removed.extend(invalidated);
        let expired = self.remove_hashes_raw(expired_hashes);
        let expired_hashes =
            expired.iter().map(|transaction| *transaction.hash()).collect::<HashSet<_>>();
        removed.extend(expired);
        for account in accounts {
            let lane_id = BaseTransactionLane::Protocol { sender: account.address };
            let stale = {
                let lane = self.lanes.entry(lane_id).or_default();
                lane.cursor = account.nonce;
                lane.exhausted = false;
                lane.transactions
                    .range(..account.nonce)
                    .map(|(_, transaction)| *transaction.hash())
                    .collect::<Vec<_>>()
            };
            removed.extend(self.remove_hashes_raw(&stale));
            self.payer_balances.insert(account.address, account.balance);
        }
        removed.sort_unstable_by_key(|transaction| *transaction.hash());
        removed.dedup_by_key(|transaction| *transaction.hash());
        self.rebalance_funding_incremental(&journal);
        let terminals = removed
            .iter()
            .map(|transaction| {
                let terminal = if mined_hashes.contains(transaction.hash()) {
                    LaneTerminalEvent::Mined { block_hash }
                } else if invalidated_hashes.contains(transaction.hash()) {
                    LaneTerminalEvent::Invalid
                } else if expired_hashes.contains(transaction.hash()) {
                    LaneTerminalEvent::Expired
                } else {
                    LaneTerminalEvent::Committed
                };
                (*transaction.hash(), terminal)
            })
            .collect();
        let transitions = self.transition_batch(LaneTransitionCause::Mining, &before, terminals);
        LaneCanonicalUpdateOutcome { removed, mined, transitions }
    }

    /// Commits canonical identities, including identities not currently present by hash.
    ///
    /// Committing a nonce identity advances its exact lane and removes all entries through that
    /// nonce. Committing a replay identity removes only that replay entry.
    pub fn commit(&mut self, identities: &[BaseTransactionIdentity]) -> LaneCommitOutcome<T> {
        self.commit_with_terminal(
            identities,
            LaneTransitionCause::Commit,
            LaneTerminalEvent::Committed,
        )
    }

    /// Commits canonical identities as mined in `block_hash`.
    pub fn commit_mined(
        &mut self,
        identities: &[BaseTransactionIdentity],
        block_hash: B256,
    ) -> LaneCommitOutcome<T> {
        self.commit_with_terminal(
            identities,
            LaneTransitionCause::Mining,
            LaneTerminalEvent::Mined { block_hash },
        )
    }

    fn commit_with_terminal(
        &mut self,
        identities: &[BaseTransactionIdentity],
        cause: LaneTransitionCause,
        terminal: LaneTerminalEvent,
    ) -> LaneCommitOutcome<T> {
        let before = self.transition_snapshot();
        let mut journal = self.mutation_journal();
        for identity in identities {
            self.seed_journal_identity(&mut journal, *identity);
        }
        self.expand_funding_closure(&mut journal);
        let mut removed = Vec::new();
        for identity in identities {
            match *identity {
                BaseTransactionIdentity::Replay { .. } => {
                    if let Some(transaction) = self.remove_indexed(*identity) {
                        removed.push(transaction);
                    }
                }
                BaseTransactionIdentity::Nonce { lane, nonce } => {
                    let committed = self
                        .lanes
                        .entry(lane)
                        .or_default()
                        .transactions
                        .range(..=nonce)
                        .map(|(stored_nonce, _)| BaseTransactionIdentity::Nonce {
                            lane,
                            nonce: *stored_nonce,
                        })
                        .collect::<Vec<_>>();
                    let lane = self.lanes.get_mut(&lane).expect("lane exists");
                    if nonce == u64::MAX {
                        lane.exhausted = true;
                    } else {
                        lane.cursor = lane.cursor.max(nonce + 1);
                    }
                    removed.extend(
                        committed.into_iter().filter_map(|stored| self.remove_indexed(stored)),
                    );
                }
            }
        }
        self.rebalance_funding_incremental(&journal);
        let terminals = Self::terminals_for(&removed, terminal);
        let transitions = self.transition_batch(cause, &before, terminals);
        let (promoted, _) = Self::pending_changes_from_batch(&transitions);
        let funding_transitions = Self::funding_transitions_from_batch(&transitions);
        LaneCommitOutcome { removed, promoted, funding_transitions, transitions }
    }

    /// Removes every transaction belonging to a physical sender.
    pub fn remove_by_sender(&mut self, sender: Address) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.remove_by_sender_with_outcome(sender, LaneRemovalReason::Removed).removed
    }

    /// Removes every transaction for a sender with a complete lifecycle batch.
    pub fn remove_by_sender_with_outcome(
        &mut self,
        sender: Address,
        reason: LaneRemovalReason,
    ) -> LaneRemovalOutcome<T> {
        let hashes =
            self.senders.get(&sender).cloned().unwrap_or_default().into_iter().collect::<Vec<_>>();
        self.remove_exact_with_outcome(&hashes, reason)
    }

    /// Evicts transactions until all configured Reth subpool limits hold.
    pub fn enforce_limits(&mut self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.enforce_limits_with_outcome().removed
    }

    /// Enforces configured limits and returns one eviction lifecycle batch.
    pub fn enforce_limits_with_outcome(&mut self) -> LaneRemovalOutcome<T> {
        self.enforce_limits_by(|transaction| {
            (
                transaction.transaction.max_fee_per_gas(),
                transaction.transaction.priority_fee_or_price(),
                *transaction.hash(),
            )
        })
    }

    /// Enforces limits using the configured production ordering and current base fee.
    pub fn enforce_limits_with_ordering<O>(&mut self, ordering: &O) -> LaneRemovalOutcome<T>
    where
        O: TransactionOrdering<Transaction = T>,
    {
        let base_fee = self.base_fee;
        self.enforce_limits_by(|transaction| {
            BestTransactionPriority::new(ordering, transaction, base_fee)
        })
    }

    /// Enforces limits while extending a sparse journal with affected dependencies.
    pub fn enforce_limits_with_ordering_journal<O>(
        &mut self,
        ordering: &O,
        journal: &mut LaneMutationJournal<T>,
        size: &mut LaneStoreSize,
    ) -> Vec<Arc<ValidPoolTransaction<T>>>
    where
        O: TransactionOrdering<Transaction = T>,
    {
        let base_fee = self.base_fee;
        let mut discarded = Vec::new();
        while let Some(exceeded) = self.exceeded_subpool(*size) {
            let victim = self.eviction_victim(exceeded, &|transaction| {
                BestTransactionPriority::new(ordering, transaction, base_fee)
            });
            let Some(victim) = victim else { break };
            let mut removal = self.mutation_journal();
            self.journal_removal(&mut removal, victim);
            for (hash, before) in &removal.before {
                journal.before.entry(*hash).or_insert_with(|| before.clone());
            }
            discarded.extend(self.remove_descendants_raw(&[victim]));
            self.rebalance_funding_incremental(&removal);
            *size = self.size_after_journal(*size, &removal);
        }
        discarded
    }

    fn enforce_limits_by<K, F>(&mut self, priority: F) -> LaneRemovalOutcome<T>
    where
        K: Ord,
        F: Fn(&Arc<ValidPoolTransaction<T>>) -> K,
    {
        let before = self.transition_snapshot();
        let mut discarded = Vec::new();
        loop {
            let size = self.size();
            let exceeded = self.exceeded_subpool(size);
            let Some(exceeded) = exceeded else { break };
            let victim = self.eviction_victim(exceeded, &priority);
            let Some(victim) = victim else { break };
            let mut journal = self.mutation_journal();
            self.journal_removal(&mut journal, victim);
            discarded.extend(self.remove_descendants_raw(&[victim]));
            self.rebalance_funding_incremental(&journal);
        }
        self.removal_outcome(before, discarded, LaneRemovalReason::Evicted)
    }

    fn exceeded_subpool(&self, size: LaneStoreSize) -> Option<LaneTransactionState> {
        [
            (
                LaneTransactionState::Pending,
                self.config.pending_limit,
                size.pending,
                size.pending_size,
            ),
            (
                LaneTransactionState::BaseFee,
                self.config.basefee_limit,
                size.basefee,
                size.basefee_size,
            ),
        ]
        .into_iter()
        .find_map(|(state, limit, count, bytes)| limit.is_exceeded(count, bytes).then_some(state))
        .or_else(|| {
            self.config
                .queued_limit
                .is_exceeded(size.queued, size.queued_size)
                .then_some(LaneTransactionState::Queued(LaneGap::Missing { expected: 0, found: 0 }))
        })
    }

    fn eviction_victim<K, F>(&self, exceeded: LaneTransactionState, priority: &F) -> Option<TxHash>
    where
        K: Ord,
        F: Fn(&Arc<ValidPoolTransaction<T>>) -> K,
    {
        let classified = self
            .classified_transactions()
            .map(|(state, transaction)| (*transaction.hash(), state))
            .collect::<B256Map<_>>();
        let mut candidates = Vec::new();
        for lane in self.lanes.values() {
            let live = lane.live().collect::<Vec<_>>();
            for (index, transaction) in live.iter().enumerate() {
                let state = classified[transaction.hash()];
                if !Self::same_subpool(state, exceeded) {
                    continue;
                }
                let suffix_len = live.len() - index;
                candidates.push((suffix_len, *transaction));
            }
        }
        for transaction in self.replays.values() {
            let state = classified[transaction.hash()];
            if Self::same_subpool(state, exceeded) {
                candidates.push((1, transaction));
            }
        }
        candidates
            .into_iter()
            .min_by_key(|(suffix_len, transaction)| {
                let local = self
                    .config
                    .local_transactions_config
                    .is_local(transaction.origin, transaction.sender_ref());
                (local, *suffix_len != 1, *suffix_len, priority(transaction), *transaction.hash())
            })
            .map(|(_, transaction)| *transaction.hash())
    }

    /// Returns one globally ordered snapshot iterator across every executable lane and replay.
    pub fn best_transactions<O>(&self, ordering: O, base_fee: u64) -> BestLaneTransactions<T, O>
    where
        O: TransactionOrdering<Transaction = T>,
    {
        BestLaneTransactions::new(self, ordering, base_fee)
    }

    fn ensure_sender_capacity(
        &self,
        transaction: &ValidPoolTransaction<T>,
        lane_cursor: u64,
        replacing: bool,
        additional_sender_slots: usize,
    ) -> PoolResult<()> {
        if replacing
            || self
                .config
                .local_transactions_config
                .is_local(transaction.origin, transaction.sender_ref())
        {
            return Ok(());
        }
        let sender = transaction.sender();
        let has_capacity =
            self.senders.get(&sender).map_or(additional_sender_slots, |transactions| {
                transactions.len().saturating_add(additional_sender_slots)
            }) < self.config.max_account_slots;
        let fills_lane_gap = match transaction.transaction.identity() {
            BaseTransactionIdentity::Replay { .. } => false,
            BaseTransactionIdentity::Nonce { lane, nonce } => {
                let stored = self.lanes.get(&lane);
                let cursor = stored.map_or(lane_cursor, |stored| stored.cursor);
                nonce == cursor
                    || stored.is_some_and(|stored| {
                        stored.transactions.range(nonce.saturating_add(1)..).next().is_some()
                    })
            }
        };
        if !has_capacity && !fills_lane_gap {
            return Err(PoolError::new(
                *transaction.hash(),
                PoolErrorKind::SpammerExceededCapacity(sender),
            ));
        }
        Ok(())
    }

    fn index(
        &mut self,
        identity: BaseTransactionIdentity,
        transaction: Arc<ValidPoolTransaction<T>>,
    ) {
        let hash = *transaction.hash();
        self.senders.entry(transaction.sender()).or_default().insert(hash);
        self.sender_ids.entry(transaction.sender_id()).or_default().insert(hash);
        if let Some(authority_ids) = &transaction.authority_ids {
            for authority_id in authority_ids {
                self.authorities.entry(*authority_id).or_default().insert(hash);
            }
        }
        if let Some(funding) = transaction.transaction.validated_funding() {
            self.payer_hashes.entry(funding.payer()).or_default().insert(hash);
        }
        self.hashes.insert(hash, Arc::clone(&transaction));
        self.identities.insert(identity, transaction);
    }

    fn remove_indexed(
        &mut self,
        identity: BaseTransactionIdentity,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        let transaction = self.identities.remove(&identity)?;
        self.unregister_funding_candidate(*transaction.hash());
        match identity {
            BaseTransactionIdentity::Nonce { lane, .. } => {
                if self.lane_funding_heads.get(&lane) == Some(transaction.hash()) {
                    self.lane_funding_heads.remove(&lane);
                }
            }
            BaseTransactionIdentity::Replay { .. } => {
                self.replay_funding_candidates.remove(transaction.hash());
            }
        }
        self.release_reservation(transaction.hash());
        match identity {
            BaseTransactionIdentity::Nonce { lane, nonce } => {
                self.lanes.get_mut(&lane)?.transactions.remove(&nonce);
            }
            BaseTransactionIdentity::Replay { replay_id } => {
                self.replays.remove(&replay_id);
            }
        }
        self.hashes.remove(transaction.hash());
        if let Some(hashes) = self.sender_ids.get_mut(&transaction.sender_id()) {
            hashes.remove(transaction.hash());
            if hashes.is_empty() {
                self.sender_ids.remove(&transaction.sender_id());
            }
        }
        if let Some(authority_ids) = &transaction.authority_ids {
            for authority_id in authority_ids {
                if let Some(hashes) = self.authorities.get_mut(authority_id) {
                    hashes.remove(transaction.hash());
                    if hashes.is_empty() {
                        self.authorities.remove(authority_id);
                    }
                }
            }
        }
        if let Some(funding) = transaction.transaction.validated_funding()
            && let Some(hashes) = self.payer_hashes.get_mut(&funding.payer())
        {
            hashes.remove(transaction.hash());
            if hashes.is_empty() {
                self.payer_hashes.remove(&funding.payer());
            }
        }
        let sender = transaction.sender();
        if let Some(hashes) = self.senders.get_mut(&sender) {
            hashes.remove(transaction.hash());
            if hashes.is_empty() {
                self.senders.remove(&sender);
            }
        }
        Some(transaction)
    }

    fn remove_hashes_raw(&mut self, hashes: &[TxHash]) -> Vec<StoredTransaction<T>> {
        hashes
            .iter()
            .filter_map(|hash| {
                self.hashes.get(hash).map(|transaction| transaction.transaction.identity())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|identity| self.remove_indexed(identity))
            .collect()
    }

    fn prune_raw(&mut self, hashes: &[TxHash]) -> Vec<StoredTransaction<T>> {
        let mut identities = hashes
            .iter()
            .filter_map(|hash| self.hashes.get(hash))
            .map(|transaction| transaction.transaction.identity())
            .collect::<Vec<_>>();
        identities.sort_unstable_by_key(|identity| Self::identity_sort_key(*identity));
        identities.dedup();
        let mut removed = Vec::new();
        for identity in identities {
            if let BaseTransactionIdentity::Nonce { lane, nonce } = identity
                && self
                    .lanes
                    .get(&lane)
                    .is_some_and(|stored| !stored.exhausted && stored.cursor == nonce)
            {
                let stored = self.lanes.get_mut(&lane).expect("lane exists");
                if let Some(cursor) = nonce.checked_add(1) {
                    stored.cursor = cursor;
                } else {
                    stored.exhausted = true;
                }
            }
            if let Some(transaction) = self.remove_indexed(identity) {
                removed.push(transaction);
            }
        }
        removed
    }

    fn remove_descendants_raw(&mut self, hashes: &[TxHash]) -> Vec<StoredTransaction<T>> {
        let mut identities = Vec::new();
        for hash in hashes {
            let Some(transaction) = self.hashes.get(hash) else { continue };
            match transaction.transaction.identity() {
                BaseTransactionIdentity::Replay { replay_id } => {
                    identities.push(BaseTransactionIdentity::Replay { replay_id });
                }
                BaseTransactionIdentity::Nonce { lane, nonce } => {
                    if let Some(stored) = self.lanes.get(&lane) {
                        identities.extend(stored.transactions.range(nonce..).map(
                            |(stored_nonce, _)| BaseTransactionIdentity::Nonce {
                                lane,
                                nonce: *stored_nonce,
                            },
                        ));
                    }
                }
            }
        }
        identities.sort_unstable_by_key(|identity| Self::identity_sort_key(*identity));
        identities.dedup();
        identities.into_iter().filter_map(|identity| self.remove_indexed(identity)).collect()
    }

    fn update_payer_balance(
        &mut self,
        payer: Address,
        balance: Option<U256>,
    ) -> PayerBalanceUpdateOutcome<T> {
        let mut journal = self.mutation_journal();
        self.journal_payer_change(&mut journal, payer);
        let previous_balance = match balance {
            Some(balance) => self.payer_balances.insert(payer, balance),
            None => self.payer_balances.remove(&payer),
        };
        self.rebalance_funding_incremental(&journal);
        let transitions =
            self.transitions_since_journal(journal, LaneTransitionCause::PayerBalance, Vec::new());
        let (promoted, demoted) = Self::pending_changes_from_batch(&transitions);
        let funding_transitions = Self::funding_transitions_from_batch(&transitions);
        PayerBalanceUpdateOutcome {
            payer,
            previous_balance,
            balance,
            promoted,
            demoted,
            funding_transitions,
            transitions,
        }
    }

    fn funding_state_for(
        &self,
        transaction: &Arc<ValidPoolTransaction<T>>,
    ) -> TransactionFundingState {
        if self.funding_reservations.contains_key(transaction.hash()) {
            return TransactionFundingState::Reserved(
                *transaction
                    .transaction
                    .validated_funding()
                    .expect("reservations require validated funding"),
            );
        }
        TransactionFundingState::Waiting(self.funding_wait_reason(transaction))
    }

    fn funding_wait_reason(&self, transaction: &Arc<ValidPoolTransaction<T>>) -> FundingWaitReason {
        if let BaseTransactionIdentity::Nonce { lane, nonce } = transaction.transaction.identity()
            && let Some(reason) = self.lane_funding_blocker(lane, nonce)
        {
            return reason;
        }
        let Some(funding) = transaction.transaction.validated_funding() else {
            return FundingWaitReason::MissingValidatedFunding;
        };
        let payer = funding.payer();
        let Some(balance) = self.payer_balances.get(&payer).copied() else {
            return FundingWaitReason::UnknownPayerBalance { payer };
        };
        FundingWaitReason::InsufficientPayerBalance {
            payer,
            required: funding.max_cost(),
            available: balance.saturating_sub(self.payer_reserved(payer)),
        }
    }

    fn lane_funding_blocker(
        &self,
        lane: BaseTransactionLane,
        target_nonce: u64,
    ) -> Option<FundingWaitReason> {
        let lane = self.lanes.get(&lane)?;
        let mut expected = lane.cursor;
        for (&nonce, transaction) in lane.transactions.range(lane.cursor..) {
            if nonce != expected {
                return (target_nonce >= nonce)
                    .then_some(FundingWaitReason::LaneGap { expected, found: nonce });
            }
            if nonce == target_nonce {
                return None;
            }
            if !self.funding_reservations.contains_key(transaction.hash()) {
                return (target_nonce > nonce)
                    .then_some(FundingWaitReason::BlockedByLaneFunding { ancestor: nonce });
            }
            expected = expected.saturating_add(1);
        }
        None
    }

    fn is_funding_head(&self, identity: BaseTransactionIdentity) -> bool {
        match identity {
            BaseTransactionIdentity::Replay { .. } => true,
            BaseTransactionIdentity::Nonce { lane, nonce } => {
                self.lane_funding_blocker(lane, nonce).is_none()
            }
        }
    }

    fn try_reserve(&mut self, transaction: &Arc<ValidPoolTransaction<T>>) -> bool {
        if self.funding_reservations.contains_key(transaction.hash()) {
            return true;
        }
        let Some(funding) = transaction.transaction.validated_funding().copied() else {
            return false;
        };
        let payer = funding.payer();
        let Some(balance) = self.payer_balances.get(&payer).copied() else {
            return false;
        };
        let reserved = self.payer_reserved(payer);
        let Some(updated) = reserved.checked_add(funding.max_cost()) else {
            return false;
        };
        if updated > balance {
            return false;
        }
        if !updated.is_zero() {
            self.payer_reserved.insert(payer, updated);
        }
        self.funding_reservations.insert(
            *transaction.hash(),
            FundingReservation { payer, max_cost: funding.max_cost() },
        );
        self.payer_reservations.entry(payer).or_default().insert(*transaction.hash());
        true
    }

    fn release_reservation(&mut self, hash: &TxHash) -> bool {
        let Some(reservation) = self.funding_reservations.remove(hash) else {
            return false;
        };
        if let Some(hashes) = self.payer_reservations.get_mut(&reservation.payer) {
            hashes.remove(hash);
            if hashes.is_empty() {
                self.payer_reservations.remove(&reservation.payer);
            }
        }
        let reserved = self.payer_reserved(reservation.payer);
        debug_assert!(reserved >= reservation.max_cost);
        let updated = reserved.saturating_sub(reservation.max_cost);
        if updated.is_zero() {
            self.payer_reserved.remove(&reservation.payer);
        } else {
            self.payer_reserved.insert(reservation.payer, updated);
        }
        true
    }

    fn release_lane_suffix(&mut self, hash: TxHash) {
        let Some(transaction) = self.hashes.get(&hash) else { return };
        let BaseTransactionIdentity::Nonce { lane, nonce } = transaction.transaction.identity()
        else {
            self.release_reservation(&hash);
            return;
        };
        let hashes = self
            .lanes
            .get(&lane)
            .into_iter()
            .flat_map(|stored| stored.transactions.range(nonce..))
            .map(|(_, transaction)| *transaction.hash())
            .collect::<Vec<_>>();
        for hash in hashes {
            self.release_reservation(&hash);
        }
    }

    fn rebalance_funding_incremental(&mut self, journal: &LaneMutationJournal<T>) {
        self.clear_funding_candidates(journal);

        let invalid = journal
            .payers
            .iter()
            .flat_map(|payer| self.payer_reservations.get(payer).into_iter().flatten())
            .copied()
            .filter(|hash| {
                let Some(reservation) = self.funding_reservations.get(hash) else { return true };
                self.hashes
                    .get(hash)
                    .and_then(|transaction| transaction.transaction.validated_funding())
                    .is_none_or(|funding| {
                        funding.payer() != reservation.payer
                            || funding.max_cost() != reservation.max_cost
                    })
            })
            .collect::<Vec<_>>();
        for hash in invalid {
            self.release_reservation(&hash);
        }

        let mut invalid_suffixes = Vec::new();
        for lane_id in &journal.lanes {
            #[cfg(test)]
            self.incremental_lane_scans.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(lane) = self.lanes.get(lane_id) else { continue };
            let mut expected = lane.cursor;
            let mut blocked = false;
            for (&nonce, transaction) in lane.transactions.range(lane.cursor..) {
                if blocked || nonce != expected {
                    blocked = true;
                    if self.funding_reservations.contains_key(transaction.hash()) {
                        invalid_suffixes.push(*transaction.hash());
                    }
                    continue;
                }
                if !self.funding_reservations.contains_key(transaction.hash()) {
                    blocked = true;
                }
                expected = expected.saturating_add(1);
            }
        }
        for hash in invalid_suffixes {
            self.release_reservation(&hash);
        }

        for payer in &journal.payers {
            #[cfg(test)]
            self.incremental_payer_scans.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            loop {
                let reserved = self.payer_reserved(*payer);
                let balance = self.payer_balances.get(payer).copied().unwrap_or_default();
                if reserved <= balance {
                    break;
                }
                let victim = self
                    .payer_reservations
                    .get(payer)
                    .into_iter()
                    .flatten()
                    .filter_map(|hash| self.hashes.get(hash))
                    .min_by_key(|transaction| Self::funding_priority(transaction))
                    .map(|transaction| *transaction.hash())
                    .expect("an overdrawn payer has reservations");
                self.release_lane_suffix(victim);
            }
        }

        self.refresh_funding_candidates(journal);
        let mut blocked = HashSet::new();
        loop {
            let candidate = journal
                .payers
                .iter()
                .flat_map(|payer| self.payer_funding_candidates.get(payer).into_iter().flatten())
                .filter(|hash| !blocked.contains(*hash))
                .filter_map(|hash| self.hashes.get(hash))
                .max_by_key(|transaction| Self::funding_priority(transaction))
                .cloned();
            let Some(transaction) = candidate else { break };
            let hash = *transaction.hash();
            self.unregister_funding_candidate(hash);
            if self.try_reserve(&transaction) {
                match transaction.transaction.identity() {
                    BaseTransactionIdentity::Nonce { lane, .. } => {
                        self.refresh_lane_funding_candidate(lane);
                    }
                    BaseTransactionIdentity::Replay { .. } => {
                        self.replay_funding_candidates.remove(&hash);
                    }
                }
            } else {
                blocked.insert(hash);
                self.register_funding_candidate(&transaction);
            }
        }
    }

    fn clear_funding_candidates(&mut self, journal: &LaneMutationJournal<T>) {
        let lane_hashes = journal
            .lanes
            .iter()
            .filter_map(|lane| self.lane_funding_heads.remove(lane))
            .collect::<Vec<_>>();
        for hash in lane_hashes {
            self.unregister_funding_candidate(hash);
        }
        let replay_hashes = journal
            .replays
            .iter()
            .filter(|hash| self.replay_funding_candidates.remove(*hash))
            .copied()
            .collect::<Vec<_>>();
        for hash in replay_hashes {
            self.unregister_funding_candidate(hash);
        }
    }

    fn refresh_funding_candidates(&mut self, journal: &LaneMutationJournal<T>) {
        for lane in &journal.lanes {
            self.refresh_lane_funding_candidate(*lane);
        }
        for hash in &journal.replays {
            if let Some(transaction) = self.hashes.get(hash).cloned()
                && !self.funding_reservations.contains_key(hash)
            {
                self.replay_funding_candidates.insert(*hash);
                self.register_funding_candidate(&transaction);
            }
        }
    }

    fn refresh_lane_funding_candidate(&mut self, lane_id: BaseTransactionLane) {
        if let Some(previous) = self.lane_funding_heads.remove(&lane_id) {
            self.unregister_funding_candidate(previous);
        }
        let candidate = self.lanes.get(&lane_id).and_then(|lane| {
            let mut expected = lane.cursor;
            for (&nonce, transaction) in lane.transactions.range(lane.cursor..) {
                if nonce != expected {
                    return None;
                }
                if !self.funding_reservations.contains_key(transaction.hash()) {
                    return Some(Arc::clone(transaction));
                }
                expected = expected.saturating_add(1);
            }
            None
        });
        if let Some(transaction) = candidate {
            self.lane_funding_heads.insert(lane_id, *transaction.hash());
            self.register_funding_candidate(&transaction);
        }
    }

    fn register_funding_candidate(&mut self, transaction: &Arc<ValidPoolTransaction<T>>) {
        let Some(funding) = transaction.transaction.validated_funding() else { return };
        let hash = *transaction.hash();
        self.funding_candidate_payers.insert(hash, funding.payer());
        self.payer_funding_candidates.entry(funding.payer()).or_default().insert(hash);
    }

    fn unregister_funding_candidate(&mut self, hash: TxHash) {
        let Some(payer) = self.funding_candidate_payers.remove(&hash) else { return };
        if let Some(hashes) = self.payer_funding_candidates.get_mut(&payer) {
            hashes.remove(&hash);
            if hashes.is_empty() {
                self.payer_funding_candidates.remove(&payer);
            }
        }
    }

    fn funding_priority(transaction: &Arc<ValidPoolTransaction<T>>) -> FundingPriority {
        FundingPriority {
            tip: transaction.transaction.priority_fee_or_price(),
            fee_cap: transaction.transaction.max_fee_per_gas(),
            timestamp: Reverse(transaction.timestamp),
            hash: *transaction.hash(),
        }
    }

    fn transition_snapshot(&self) -> B256Map<TransitionSnapshot<T>> {
        self.classified_transactions()
            .map(|(state, transaction)| {
                (
                    *transaction.hash(),
                    TransitionSnapshot {
                        transaction: Arc::clone(transaction),
                        state,
                        funding: self.funding_state_for(transaction),
                    },
                )
            })
            .collect()
    }

    fn transition_snapshot_for_hash(&self, hash: &TxHash) -> Option<TransitionSnapshot<T>> {
        let transaction = self.hashes.get(hash)?;
        let state = match transaction.transaction.identity() {
            BaseTransactionIdentity::Nonce { lane, .. } => self
                .classified_lane(self.lanes.get(&lane)?)
                .into_iter()
                .find_map(|(state, candidate)| (candidate.hash() == hash).then_some(state))?,
            BaseTransactionIdentity::Replay { .. } => {
                if transaction.transaction.max_fee_per_gas() < u128::from(self.base_fee) {
                    LaneTransactionState::BaseFee
                } else if let TransactionFundingState::Waiting(reason) =
                    self.funding_state_for(transaction)
                {
                    LaneTransactionState::Funding(reason)
                } else {
                    LaneTransactionState::Pending
                }
            }
        };
        Some(TransitionSnapshot {
            transaction: Arc::clone(transaction),
            state,
            funding: self.funding_state_for(transaction),
        })
    }

    fn journal_hash(&self, journal: &mut LaneMutationJournal<T>, hash: TxHash) {
        journal.before.entry(hash).or_insert_with(|| self.transition_snapshot_for_hash(&hash));
    }

    fn seed_journal_identity(
        &self,
        journal: &mut LaneMutationJournal<T>,
        identity: BaseTransactionIdentity,
    ) {
        match identity {
            BaseTransactionIdentity::Nonce { lane, .. } => {
                journal.lanes.insert(lane);
            }
            BaseTransactionIdentity::Replay { replay_id } => {
                if let Some(transaction) = self.replays.get(&replay_id) {
                    journal.replays.insert(*transaction.hash());
                }
            }
        }
    }

    fn expand_funding_closure(&self, journal: &mut LaneMutationJournal<T>) {
        loop {
            let next_lane =
                journal.lanes.iter().find(|lane| !journal.scanned_lanes.contains(lane)).copied();
            if let Some(lane) = next_lane {
                journal.scanned_lanes.insert(lane);
                #[cfg(test)]
                self.incremental_lane_scans.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Some(stored) = self.lanes.get(&lane) {
                    for transaction in stored.transactions.values() {
                        self.journal_hash(journal, *transaction.hash());
                        if let Some(funding) = transaction.transaction.validated_funding() {
                            journal.payers.insert(funding.payer());
                        }
                    }
                }
                continue;
            }

            let next_payer = journal
                .payers
                .iter()
                .find(|payer| !journal.scanned_payers.contains(*payer))
                .copied();
            if let Some(payer) = next_payer {
                journal.scanned_payers.insert(payer);
                #[cfg(test)]
                self.incremental_payer_scans.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Some(hashes) = self.payer_hashes.get(&payer) {
                    for hash in hashes {
                        self.journal_hash(journal, *hash);
                        if let Some(transaction) = self.hashes.get(hash) {
                            self.seed_journal_identity(journal, transaction.transaction.identity());
                        }
                    }
                }
                continue;
            }

            let next_replay = journal
                .replays
                .iter()
                .find(|hash| !journal.scanned_replays.contains(*hash))
                .copied();
            if let Some(hash) = next_replay {
                journal.scanned_replays.insert(hash);
                self.journal_hash(journal, hash);
                if let Some(transaction) = self.hashes.get(&hash)
                    && let Some(funding) = transaction.transaction.validated_funding()
                {
                    journal.payers.insert(funding.payer());
                }
                continue;
            }

            break;
        }
    }

    fn journal_removal(&self, journal: &mut LaneMutationJournal<T>, hash: TxHash) {
        let Some(transaction) = self.hashes.get(&hash) else { return };
        let identity = transaction.transaction.identity();
        self.seed_journal_identity(journal, identity);
        if let Some(funding) = transaction.transaction.validated_funding() {
            journal.payers.insert(funding.payer());
        }
        self.expand_funding_closure(journal);
    }

    fn add_size_entry(
        size: &mut LaneStoreSize,
        state: LaneTransactionState,
        transaction: &Arc<ValidPoolTransaction<T>>,
    ) {
        size.total += 1;
        let bytes = transaction.transaction.size();
        match state {
            LaneTransactionState::Pending => {
                size.pending += 1;
                size.pending_size += bytes;
            }
            LaneTransactionState::BaseFee => {
                size.basefee += 1;
                size.basefee_size += bytes;
            }
            LaneTransactionState::Funding(_) | LaneTransactionState::Queued(_) => {
                size.queued += 1;
                size.queued_size += bytes;
            }
        }
    }

    fn remove_size_entry(
        size: &mut LaneStoreSize,
        state: LaneTransactionState,
        transaction: &Arc<ValidPoolTransaction<T>>,
    ) {
        size.total = size.total.saturating_sub(1);
        let bytes = transaction.transaction.size();
        match state {
            LaneTransactionState::Pending => {
                size.pending = size.pending.saturating_sub(1);
                size.pending_size = size.pending_size.saturating_sub(bytes);
            }
            LaneTransactionState::BaseFee => {
                size.basefee = size.basefee.saturating_sub(1);
                size.basefee_size = size.basefee_size.saturating_sub(bytes);
            }
            LaneTransactionState::Funding(_) | LaneTransactionState::Queued(_) => {
                size.queued = size.queued.saturating_sub(1);
                size.queued_size = size.queued_size.saturating_sub(bytes);
            }
        }
    }

    fn transition_batch(
        &self,
        cause: LaneTransitionCause,
        before: &B256Map<TransitionSnapshot<T>>,
        terminals: B256Map<LaneTerminalEvent>,
    ) -> LaneTransitionBatch<T> {
        self.transition_batch_with_transactions(cause, before, terminals, &B256Map::default())
    }

    fn transition_batch_with_transactions(
        &self,
        cause: LaneTransitionCause,
        before: &B256Map<TransitionSnapshot<T>>,
        terminals: B256Map<LaneTerminalEvent>,
        terminal_transactions: &B256Map<Arc<ValidPoolTransaction<T>>>,
    ) -> LaneTransitionBatch<T> {
        let after = self.transition_snapshot();
        let mut hashes =
            before.keys().chain(after.keys()).chain(terminals.keys()).copied().collect::<Vec<_>>();
        hashes.sort_unstable();
        hashes.dedup();
        let transitions = hashes
            .into_iter()
            .filter_map(|hash| {
                let previous = before.get(&hash);
                let current = after.get(&hash);
                let terminal = terminals.get(&hash).copied();
                let previous_state = previous.map(|snapshot| snapshot.state);
                let current_state = current.map(|snapshot| snapshot.state);
                let previous_funding = previous.map(|snapshot| snapshot.funding);
                let current_funding = current.map(|snapshot| snapshot.funding);
                (previous_state != current_state
                    || previous_funding != current_funding
                    || terminal.is_some())
                .then(|| LaneTransactionTransition {
                    transaction: Arc::clone(
                        current
                            .map(|snapshot| &snapshot.transaction)
                            .or_else(|| previous.map(|snapshot| &snapshot.transaction))
                            .or_else(|| terminal_transactions.get(&hash))
                            .expect("transition hash exists in a snapshot"),
                    ),
                    previous_state,
                    current_state,
                    previous_funding,
                    current_funding,
                    terminal,
                })
            })
            .collect();
        LaneTransitionBatch { cause, transitions }
    }

    fn funding_transitions_from_batch(batch: &LaneTransitionBatch<T>) -> Vec<FundingTransition<T>> {
        batch
            .transitions
            .iter()
            .filter_map(|transition| {
                let current = transition.current_funding?;
                (transition.previous_funding != Some(current)).then(|| FundingTransition {
                    transaction: Arc::clone(&transition.transaction),
                    previous: transition.previous_funding,
                    current,
                })
            })
            .collect()
    }

    fn pending_changes_from_batch(
        batch: &LaneTransitionBatch<T>,
    ) -> (Vec<StoredTransaction<T>>, Vec<DemotedTransaction<T>>) {
        let mut promoted = Vec::new();
        let mut demoted = Vec::new();
        for transition in &batch.transitions {
            match (transition.previous_state, transition.current_state) {
                (Some(previous), Some(LaneTransactionState::Pending))
                    if previous != LaneTransactionState::Pending =>
                {
                    promoted.push(Arc::clone(&transition.transaction));
                }
                (Some(LaneTransactionState::Pending), Some(LaneTransactionState::BaseFee)) => {
                    demoted.push((Arc::clone(&transition.transaction), SubPool::BaseFee));
                }
                (
                    Some(LaneTransactionState::Pending),
                    Some(LaneTransactionState::Funding(_) | LaneTransactionState::Queued(_)),
                ) => {
                    demoted.push((Arc::clone(&transition.transaction), SubPool::Queued));
                }
                _ => {}
            }
        }
        (promoted, demoted)
    }

    fn terminals_for(
        transactions: &[StoredTransaction<T>],
        terminal: LaneTerminalEvent,
    ) -> B256Map<LaneTerminalEvent> {
        transactions.iter().map(|transaction| (*transaction.hash(), terminal)).collect()
    }

    fn removal_outcome(
        &self,
        before: B256Map<TransitionSnapshot<T>>,
        removed: Vec<StoredTransaction<T>>,
        reason: LaneRemovalReason,
    ) -> LaneRemovalOutcome<T> {
        let terminals = Self::terminals_for(&removed, reason.terminal());
        self.removal_outcome_with_terminals(before, removed, reason.cause(), terminals)
    }

    fn removal_outcome_with_terminals(
        &self,
        before: B256Map<TransitionSnapshot<T>>,
        removed: Vec<StoredTransaction<T>>,
        cause: LaneTransitionCause,
        terminals: B256Map<LaneTerminalEvent>,
    ) -> LaneRemovalOutcome<T> {
        let transitions = self.transition_batch(cause, &before, terminals);
        let (promoted, demoted) = Self::pending_changes_from_batch(&transitions);
        let funding_transitions = Self::funding_transitions_from_batch(&transitions);
        LaneRemovalOutcome { removed, promoted, demoted, funding_transitions, transitions }
    }

    fn classified_transactions(
        &self,
    ) -> impl Iterator<Item = (LaneTransactionState, &Arc<ValidPoolTransaction<T>>)> {
        #[cfg(test)]
        self.full_classification_passes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.lanes.values().flat_map(|lane| self.classified_lane(lane)).chain(
            self.replays.values().map(|transaction| {
                let state = if transaction.transaction.max_fee_per_gas() < u128::from(self.base_fee)
                {
                    LaneTransactionState::BaseFee
                } else if let TransactionFundingState::Waiting(reason) =
                    self.funding_state_for(transaction)
                {
                    LaneTransactionState::Funding(reason)
                } else {
                    LaneTransactionState::Pending
                };
                (state, transaction)
            }),
        )
    }

    fn classified_lane<'a>(
        &'a self,
        lane: &'a NonceLane<T>,
    ) -> Vec<(LaneTransactionState, &'a Arc<ValidPoolTransaction<T>>)> {
        let mut expected = lane.cursor;
        let mut blocker = None;
        lane.live()
            .map(|transaction| {
                let BaseTransactionIdentity::Nonce { nonce, .. } =
                    transaction.transaction.identity()
                else {
                    unreachable!("nonce lanes contain nonce identities")
                };
                let state = match blocker {
                    Some(gap) => LaneTransactionState::Queued(gap),
                    None if nonce != expected => {
                        let gap = LaneGap::Missing { expected, found: nonce };
                        blocker = Some(gap);
                        LaneTransactionState::Queued(gap)
                    }
                    None if transaction.transaction.max_fee_per_gas()
                        < u128::from(self.base_fee) =>
                    {
                        let gap = LaneGap::BlockedByBaseFee { ancestor: nonce };
                        blocker = Some(gap);
                        LaneTransactionState::BaseFee
                    }
                    None => match self.funding_state_for(transaction) {
                        TransactionFundingState::Reserved(_) => LaneTransactionState::Pending,
                        TransactionFundingState::Waiting(reason) => {
                            blocker = Some(LaneGap::BlockedByFunding { ancestor: nonce });
                            LaneTransactionState::Funding(reason)
                        }
                    },
                };
                expected = expected.saturating_add(1);
                (state, transaction)
            })
            .collect()
    }

    fn transactions_with_state(&self, expected: LaneTransactionState) -> Vec<StoredTransaction<T>> {
        let mut transactions = self
            .classified_transactions()
            .filter(|(state, _)| *state == expected)
            .map(|(_, transaction)| Arc::clone(transaction))
            .collect::<Vec<_>>();
        transactions.sort_unstable_by_key(|transaction| *transaction.hash());
        transactions
    }

    fn identity_sort_key(identity: BaseTransactionIdentity) -> (u8, Address, B256, u64) {
        match identity {
            BaseTransactionIdentity::Nonce {
                lane: BaseTransactionLane::Protocol { sender },
                nonce,
            } => (0, sender, B256::ZERO, nonce),
            BaseTransactionIdentity::Nonce {
                lane: BaseTransactionLane::Channel { sender, nonce_key },
                nonce,
            } => (1, sender, B256::from(nonce_key.to_be_bytes()), nonce),
            BaseTransactionIdentity::Replay { replay_id } => (2, Address::ZERO, replay_id, 0),
        }
    }

    const fn same_subpool(left: LaneTransactionState, right: LaneTransactionState) -> bool {
        matches!(
            (left, right),
            (LaneTransactionState::Pending, LaneTransactionState::Pending)
                | (LaneTransactionState::BaseFee, LaneTransactionState::BaseFee)
                | (LaneTransactionState::Funding(_), LaneTransactionState::Funding(_))
                | (LaneTransactionState::Funding(_), LaneTransactionState::Queued(_))
                | (LaneTransactionState::Queued(_), LaneTransactionState::Funding(_))
                | (LaneTransactionState::Queued(_), LaneTransactionState::Queued(_))
        )
    }

    const fn added_state(state: LaneTransactionState) -> AddedTransactionState {
        match state {
            LaneTransactionState::Pending => AddedTransactionState::Pending,
            LaneTransactionState::BaseFee => {
                AddedTransactionState::Queued(QueuedReason::InsufficientBaseFee)
            }
            LaneTransactionState::Funding(_) => {
                AddedTransactionState::Queued(QueuedReason::InsufficientBalance)
            }
            LaneTransactionState::Queued(LaneGap::Missing { .. }) => {
                AddedTransactionState::Queued(QueuedReason::NonceGap)
            }
            LaneTransactionState::Queued(
                LaneGap::BlockedByBaseFee { .. } | LaneGap::BlockedByFunding { .. },
            ) => AddedTransactionState::Queued(QueuedReason::ParkedAncestors),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum CandidateIdentity {
    Lane(BaseTransactionLane),
    Replay(B256),
}

#[derive(Debug)]
struct CandidateLane<T: BasePooledTx> {
    identity: CandidateIdentity,
    transactions: Vec<Arc<ValidPoolTransaction<T>>>,
    index: usize,
    invalidated: bool,
}

/// Snapshot iterator that globally orders executable heads across all Base lanes.
#[derive(Debug)]
pub struct BestLaneTransactions<T: BasePooledTx, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    lanes: Vec<CandidateLane<T>>,
    candidates: BinaryHeap<(BestTransactionPriority<O::PriorityValue>, usize)>,
    indexes: HashMap<CandidateIdentity, usize>,
    ordering: O,
    base_fee: u64,
    skip_blobs: bool,
}

impl<T: BasePooledTx, O> BestLaneTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    fn new(store: &LaneTransactionStore<T>, ordering: O, base_fee: u64) -> Self {
        let mut lanes: Vec<_> = store
            .lanes
            .iter()
            .filter_map(|(identity, lane)| {
                let transactions = lane
                    .classified(base_fee)
                    .into_iter()
                    .take_while(|(state, transaction)| {
                        *state == LaneTransactionState::Pending
                            && store.funding_reservations.contains_key(transaction.hash())
                    })
                    .map(|(_, transaction)| Arc::clone(transaction))
                    .collect::<Vec<_>>();
                (!transactions.is_empty()).then_some(CandidateLane {
                    identity: CandidateIdentity::Lane(*identity),
                    transactions,
                    index: 0,
                    invalidated: false,
                })
            })
            .collect();
        lanes.extend(
            store
                .replays
                .iter()
                .filter(|(_, transaction)| {
                    transaction.transaction.max_fee_per_gas() >= u128::from(base_fee)
                        && store.funding_reservations.contains_key(transaction.hash())
                })
                .map(|(replay_id, transaction)| CandidateLane {
                    identity: CandidateIdentity::Replay(*replay_id),
                    transactions: vec![Arc::clone(transaction)],
                    index: 0,
                    invalidated: false,
                }),
        );
        let indexes =
            lanes.iter().enumerate().map(|(index, lane)| (lane.identity, index)).collect();
        let candidates = BinaryHeap::from(
            lanes
                .iter()
                .enumerate()
                .map(|(index, lane)| {
                    (
                        BestTransactionPriority::new(&ordering, &lane.transactions[0], base_fee),
                        index,
                    )
                })
                .collect::<Vec<_>>(),
        );
        Self { lanes, candidates, indexes, ordering, base_fee, skip_blobs: false }
    }

    fn push_head(&mut self, index: usize) {
        let lane = &self.lanes[index];
        if lane.invalidated || lane.index >= lane.transactions.len() {
            return;
        }
        self.candidates.push((
            BestTransactionPriority::new(
                &self.ordering,
                &lane.transactions[lane.index],
                self.base_fee,
            ),
            index,
        ));
    }
}

impl<T: BasePooledTx, O> Iterator for BestLaneTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (_, index) = self.candidates.pop()?;
            let lane = &mut self.lanes[index];
            if lane.invalidated {
                continue;
            }
            let transaction = Arc::clone(&lane.transactions[lane.index]);
            if self.skip_blobs && transaction.transaction.is_eip4844() {
                lane.invalidated = true;
                continue;
            }
            lane.index += 1;
            self.push_head(index);
            return Some(transaction);
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self
            .lanes
            .iter()
            .filter(|lane| !lane.invalidated)
            .map(|lane| lane.transactions.len().saturating_sub(lane.index))
            .sum();
        (0, Some(remaining))
    }
}

impl<T: BasePooledTx, O> BestTransactions for BestLaneTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    fn mark_invalid(&mut self, transaction: &Self::Item, _kind: InvalidPoolTransactionError) {
        let identity = match transaction.transaction.identity() {
            BaseTransactionIdentity::Nonce { lane, .. } => CandidateIdentity::Lane(lane),
            BaseTransactionIdentity::Replay { replay_id } => CandidateIdentity::Replay(replay_id),
        };
        if let Some(index) = self.indexes.get(&identity) {
            self.lanes[*index].invalidated = true;
        }
    }

    fn no_updates(&mut self) {}

    fn allow_updates_out_of_order(&mut self) {}

    fn skip_blobs(&mut self) {
        self.set_skip_blobs(true);
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.skip_blobs = skip_blobs;
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use alloy_consensus::{Transaction, transaction::Recovered};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Bytes, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, BaseTransactionSigned,
        Eip8130Constants, Eip8130Signed, TxEip8130,
    };
    use base_test_utils::Account;
    use futures::StreamExt;
    use reth_primitives_traits::InMemorySize;
    use reth_transaction_pool::{
        FullTransactionEvent, PoolTransaction, TransactionEvent, TransactionListenerKind,
        TransactionOrigin,
        identifier::{SenderId, TransactionId},
        pool::PendingPool,
        test_utils::TransactionBuilder,
    };

    use super::*;
    use crate::{BaseOrdering, BasePooledTransaction, LaneEventHub};

    fn protocol_transaction(
        account: Account,
        nonce: u64,
        tip: u128,
        fee_cap: u128,
    ) -> BasePooledTransaction {
        let signed = TransactionBuilder::default()
            .signer(account.signer_b256())
            .chain_id(ChainConfig::mainnet().chain_id)
            .nonce(nonce)
            .to(Account::Bob.address())
            .value(1_000)
            .gas_limit(21_000)
            .max_priority_fee_per_gas(tip)
            .max_fee_per_gas(fee_cap)
            .into_eip1559();
        let transaction = BaseTransactionSigned::Eip1559(
            signed.as_eip1559().expect("EIP-1559 transaction").clone(),
        );
        let recovered = Recovered::new_unchecked(transaction, account.address());
        let encoded_length = recovered.encode_2718_len();
        BasePooledTransaction::new(recovered, encoded_length)
    }

    fn legacy_protocol_transaction(
        account: Account,
        nonce: u64,
        gas_price: u128,
    ) -> BasePooledTransaction {
        let signed = TransactionBuilder::default()
            .signer(account.signer_b256())
            .chain_id(ChainConfig::mainnet().chain_id)
            .nonce(nonce)
            .to(Account::Bob.address())
            .value(1_000)
            .gas_limit(21_000)
            .max_fee_per_gas(gas_price)
            .into_legacy();
        let transaction =
            BaseTransactionSigned::Legacy(signed.as_legacy().expect("legacy transaction").clone());
        let recovered = Recovered::new_unchecked(transaction, account.address());
        let encoded_length = recovered.encode_2718_len();
        BasePooledTransaction::new(recovered, encoded_length)
    }

    fn sidecar_transaction(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce: u64,
        valid_before: u64,
        tip: u128,
        fee_cap: u128,
    ) -> BasePooledTransaction {
        let transaction = TxEip8130 {
            chain_id: ChainConfig::mainnet().chain_id,
            sender: None,
            nonce_key,
            nonce_sequence: nonce,
            valid_after: 0,
            valid_before,
            max_priority_fee_per_gas: tip,
            max_fee_per_gas: fee_cap,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&transaction.sender_signature_hash()).unwrap();
        let signed = Eip8130Signed::new(
            transaction,
            Bytes::from(signature.as_bytes().to_vec()),
            Bytes::new(),
        );
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(
            ConsensusPooledTransaction::Eip8130(signed),
            signer.address(),
        ))
    }

    fn validated(
        transaction: BasePooledTransaction,
        sender_id: u64,
        timestamp: Instant,
    ) -> ValidPoolTransaction<BasePooledTransaction> {
        validated_with_funding(transaction, sender_id, timestamp, Address::ZERO, U256::ZERO)
    }

    fn validated_with_funding(
        transaction: BasePooledTransaction,
        sender_id: u64,
        timestamp: Instant,
        payer: Address,
        max_cost: U256,
    ) -> ValidPoolTransaction<BasePooledTransaction> {
        transaction.set_validated_funding(ValidatedFunding::new(payer, max_cost)).unwrap();
        ValidPoolTransaction {
            transaction_id: TransactionId::new(SenderId::from(sender_id), transaction.nonce()),
            transaction,
            propagate: true,
            timestamp,
            origin: TransactionOrigin::External,
            authority_ids: None,
        }
    }

    fn store(base_fee: u64) -> LaneTransactionStore<BasePooledTransaction> {
        let mut store = LaneTransactionStore::new(
            PoolConfig { max_account_slots: usize::MAX, ..PoolConfig::default() },
            base_fee,
        );
        store.set_payer_balance(Address::ZERO, U256::ZERO);
        store
    }

    fn hashes(
        transactions: impl IntoIterator<Item = Arc<ValidPoolTransaction<BasePooledTransaction>>>,
    ) -> Vec<TxHash> {
        let mut hashes =
            transactions.into_iter().map(|transaction| *transaction.hash()).collect::<Vec<_>>();
        hashes.sort_unstable();
        hashes
    }

    fn assert_consistent(store: &LaneTransactionStore<BasePooledTransaction>) {
        assert_eq!(store.hashes.len(), store.identities.len());
        assert_eq!(store.size().total, store.len());
        for (identity, transaction) in &store.identities {
            assert_eq!(transaction.transaction.identity(), *identity);
            assert!(Arc::ptr_eq(
                store.hashes.get(transaction.hash()).expect("hash index"),
                transaction,
            ));
            assert!(
                store
                    .senders
                    .get(&transaction.sender())
                    .is_some_and(|hashes| hashes.contains(transaction.hash()))
            );
            match *identity {
                BaseTransactionIdentity::Nonce { lane, nonce } => assert!(Arc::ptr_eq(
                    store
                        .lanes
                        .get(&lane)
                        .and_then(|lane| lane.transactions.get(&nonce))
                        .expect("lane index"),
                    transaction,
                )),
                BaseTransactionIdentity::Replay { replay_id } => assert!(Arc::ptr_eq(
                    store.replays.get(&replay_id).expect("replay index"),
                    transaction,
                )),
            }
        }
        assert_eq!(store.senders.values().map(HashSet::len).sum::<usize>(), store.len());
        assert_eq!(store.sender_ids.values().map(HashSet::len).sum::<usize>(), store.len());
        assert_eq!(store.payer_hashes.values().map(HashSet::len).sum::<usize>(), store.len());
        let mut reserved = HashMap::<Address, U256>::default();
        for (hash, reservation) in &store.funding_reservations {
            let transaction = store.hashes.get(hash).expect("reservation transaction");
            let funding = transaction.transaction.validated_funding().expect("funding metadata");
            assert_eq!(reservation.payer, funding.payer());
            assert_eq!(reservation.max_cost, funding.max_cost());
            if !reservation.max_cost.is_zero() {
                *reserved.entry(reservation.payer).or_default() += reservation.max_cost;
            }
        }
        assert_eq!(reserved, store.payer_reserved);
        assert_eq!(
            store.payer_reservations.values().map(HashSet::len).sum::<usize>(),
            store.funding_reservations.len()
        );
        for (payer, hashes) in &store.payer_reservations {
            assert!(hashes.iter().all(|hash| {
                store
                    .funding_reservations
                    .get(hash)
                    .is_some_and(|reservation| reservation.payer == *payer)
            }));
        }
        for (hash, payer) in &store.funding_candidate_payers {
            assert!(!store.funding_reservations.contains_key(hash));
            assert!(
                store
                    .payer_funding_candidates
                    .get(payer)
                    .is_some_and(|hashes| hashes.contains(hash))
            );
        }
        for (lane, hash) in &store.lane_funding_heads {
            let transaction = store.hashes.get(hash).expect("lane funding candidate");
            assert_eq!(transaction.transaction.identity().lane(), Some(*lane));
            assert!(store.funding_candidate_payers.contains_key(hash));
        }
        for hash in &store.replay_funding_candidates {
            assert!(
                store
                    .hashes
                    .get(hash)
                    .is_some_and(|transaction| transaction.transaction.identity().is_replay())
            );
            assert!(store.funding_candidate_payers.contains_key(hash));
        }
        for (payer, reserved) in &store.payer_reserved {
            assert!(*reserved <= store.payer_balance(*payer).expect("known payer balance"));
        }
        for transaction in store.pending_transactions() {
            assert!(matches!(
                store.funding_state(&transaction.transaction.identity()),
                Some(TransactionFundingState::Reserved(_))
            ));
        }
        for lane in store.lanes.values() {
            let mut expected = lane.cursor;
            let mut funding_blocked = false;
            for (&nonce, transaction) in lane.transactions.range(lane.cursor..) {
                if nonce != expected {
                    funding_blocked = true;
                }
                let is_reserved = store.funding_reservations.contains_key(transaction.hash());
                assert!(!funding_blocked || !is_reserved, "reservations must form a lane prefix");
                if !is_reserved {
                    funding_blocked = true;
                }
                expected = expected.saturating_add(1);
            }
        }
    }

    #[test]
    fn authoritative_identity_ignores_colliding_reth_transaction_ids() {
        let now = Instant::now();
        let mut store = store(0);
        let protocol = validated(protocol_transaction(Account::Alice, 0, 10, 100), 0, now);
        let signer = PrivateKeySigner::random();
        let channel = validated(sidecar_transaction(&signer, U256::from(7), 0, 0, 20, 100), 0, now);
        assert_eq!(protocol.transaction_id, channel.transaction_id);

        store.insert_validated(protocol, 0).unwrap();
        store.insert_validated(channel, 0).unwrap();

        assert_eq!(store.len(), 2);
        assert_eq!(store.size().pending, 2);
        assert_consistent(&store);
    }

    #[test]
    fn replacement_uses_reth_fee_bump_at_exact_base_identity() {
        let signer = PrivateKeySigner::random();
        let now = Instant::now();
        let mut store = store(0);
        let original =
            validated(sidecar_transaction(&signer, U256::from(9), 0, 0, 10, 100), 77, now);
        let original_id = original.transaction.identity();
        let original_hash = *original.hash();
        store.insert_validated(original, 0).unwrap();

        let underpriced =
            validated(sidecar_transaction(&signer, U256::from(9), 0, 0, 10, 105), 999, now);
        assert!(matches!(
            store.insert_validated(underpriced, 0).unwrap_err().kind,
            PoolErrorKind::ReplacementUnderpriced
        ));
        let replacement =
            validated(sidecar_transaction(&signer, U256::from(9), 0, 0, 12, 120), 999, now);
        let replacement_hash = *replacement.hash();
        let outcome = store.insert_validated(replacement, 0).unwrap();

        assert_eq!(outcome.replaced.map(|transaction| *transaction.hash()), Some(original_hash));
        assert_eq!(
            store.get_by_identity(&original_id).map(|transaction| *transaction.hash()),
            Some(replacement_hash)
        );
        assert!(!store.contains_hash(&original_hash));
        assert_consistent(&store);
    }

    #[test]
    fn gap_and_base_fee_classification_is_lane_local() {
        let now = Instant::now();
        let mut store = store(50);
        let head = validated(protocol_transaction(Account::Alice, 0, 10, 100), 1, now);
        let blocked = validated(protocol_transaction(Account::Alice, 1, 10, 40), 1, now);
        let descendant = validated(protocol_transaction(Account::Alice, 2, 10, 100), 1, now);
        let gap = validated(protocol_transaction(Account::Bob, 2, 10, 100), 2, now);
        let head_id = head.transaction.identity();
        let blocked_id = blocked.transaction.identity();
        let descendant_id = descendant.transaction.identity();
        let gap_id = gap.transaction.identity();
        store.insert_validated(head, 0).unwrap();
        store.insert_validated(blocked, 0).unwrap();
        store.insert_validated(descendant, 0).unwrap();
        store.insert_validated(gap, 0).unwrap();

        assert_eq!(store.state(&head_id), Some(LaneTransactionState::Pending));
        assert_eq!(store.state(&blocked_id), Some(LaneTransactionState::BaseFee));
        assert_eq!(
            store.state(&descendant_id),
            Some(LaneTransactionState::Queued(LaneGap::BlockedByBaseFee { ancestor: 1 }))
        );
        assert_eq!(
            store.state(&gap_id),
            Some(LaneTransactionState::Queued(LaneGap::Missing { expected: 0, found: 2 }))
        );
    }

    #[test]
    fn exact_descendant_prune_and_commit_have_distinct_semantics() {
        let now = Instant::now();
        let lane = BaseTransactionLane::Protocol { sender: Account::Alice.address() };

        let mut exact = store(0);
        let exact_txs = (0..3)
            .map(|nonce| validated(protocol_transaction(Account::Alice, nonce, 10, 100), 1, now))
            .collect::<Vec<_>>();
        let middle_hash = *exact_txs[1].hash();
        for transaction in exact_txs {
            exact.insert_validated(transaction, 0).unwrap();
        }
        assert_eq!(exact.remove_exact(&[middle_hash]).len(), 1);
        assert_eq!(exact.lane_cursor(&lane), Some(0));
        assert_eq!(exact.size().pending, 1);
        assert_eq!(exact.size().queued, 1);

        let mut descendants = store(0);
        let descendant_txs = (0..3)
            .map(|nonce| validated(protocol_transaction(Account::Alice, nonce, 10, 100), 1, now))
            .collect::<Vec<_>>();
        let middle_hash = *descendant_txs[1].hash();
        for transaction in descendant_txs {
            descendants.insert_validated(transaction, 0).unwrap();
        }
        assert_eq!(descendants.remove_with_descendants(&[middle_hash]).len(), 2);
        assert_eq!(descendants.len(), 1);

        let mut pruned = store(0);
        let prune_txs = (0..3)
            .map(|nonce| validated(protocol_transaction(Account::Alice, nonce, 10, 100), 1, now))
            .collect::<Vec<_>>();
        let head_hash = *prune_txs[0].hash();
        for transaction in prune_txs {
            pruned.insert_validated(transaction, 0).unwrap();
        }
        assert_eq!(pruned.prune(&[head_hash]).len(), 1);
        assert_eq!(pruned.lane_cursor(&lane), Some(1));
        assert_eq!(pruned.size().pending, 2);

        let committed_id = BaseTransactionIdentity::Nonce { lane, nonce: 1 };
        let outcome = pruned.commit(&[committed_id]);
        assert_eq!(outcome.removed.len(), 1);
        assert_eq!(pruned.lane_cursor(&lane), Some(2));
        assert_eq!(pruned.size().pending, 1);
        assert_consistent(&pruned);
    }

    #[test]
    fn best_invalidation_affects_only_the_exact_lane_or_replay() {
        let signer = PrivateKeySigner::random();
        let now = Instant::now();
        let mut store = store(0);
        let protocol = validated(protocol_transaction(Account::Alice, 0, 30, 100), 1, now);
        let protocol_hash = *protocol.hash();
        let channel = validated(
            sidecar_transaction(&signer, U256::from(3), 0, 0, 20, 100),
            1,
            now + Duration::from_millis(1),
        );
        let channel_hash = *channel.hash();
        let replay = validated(
            sidecar_transaction(&signer, Eip8130Constants::NONCE_KEY_MAX, 0, 1, 10, 100),
            1,
            now + Duration::from_millis(2),
        );
        let replay_hash = *replay.hash();
        store.insert_validated(protocol, 0).unwrap();
        store.insert_validated(channel, 0).unwrap();
        store.insert_validated(replay, 0).unwrap();

        let invalid = store.get_by_hash(&channel_hash).unwrap();
        let mut best = store.best_transactions(BaseOrdering::coinbase_tip(), 0);
        best.mark_invalid(&invalid, InvalidPoolTransactionError::Underpriced);
        let yielded = hashes(best);

        assert_eq!(yielded, {
            let mut expected = vec![protocol_hash, replay_hash];
            expected.sort_unstable();
            expected
        });
    }

    #[test]
    fn protocol_best_order_matches_pinned_reth_pending_pool() {
        let now = Instant::now();
        let ordering = BaseOrdering::coinbase_tip();
        let mut store = store(0);
        let mut reth = PendingPool::new(ordering.clone());
        for (account, sender_id, nonce, tip) in [
            (Account::Alice, 1, 0, 30),
            (Account::Alice, 1, 1, 5),
            (Account::Bob, 2, 0, 20),
            (Account::Bob, 2, 1, 15),
        ] {
            let transaction = protocol_transaction(account, nonce, tip, 100);
            let for_store = validated(transaction.clone(), sender_id, now);
            let for_reth = Arc::new(validated(transaction, sender_id, now));
            store.insert_validated(for_store, 0).unwrap();
            reth.add_transaction(for_reth, 0);
        }

        let ours = store
            .best_transactions(ordering, 0)
            .map(|transaction| *transaction.hash())
            .collect::<Vec<_>>();
        let theirs = reth.best().map(|transaction| *transaction.hash()).collect::<Vec<_>>();
        assert_eq!(ours, theirs);
    }

    #[test]
    fn protocol_lane_supports_mixed_self_paid_and_sponsored_funding() {
        let now = Instant::now();
        let sponsor_one = Address::repeat_byte(0x11);
        let sponsor_two = Address::repeat_byte(0x22);
        let mut store = store(0);
        store.set_payer_balance(Account::Alice.address(), U256::from(40));
        store.set_payer_balance(sponsor_one, U256::from(60));
        store.set_payer_balance(sponsor_two, U256::from(70));

        let transactions = [
            validated_with_funding(
                protocol_transaction(Account::Alice, 0, 30, 100),
                1,
                now,
                Account::Alice.address(),
                U256::from(40),
            ),
            validated_with_funding(
                protocol_transaction(Account::Alice, 1, 20, 100),
                1,
                now + Duration::from_millis(1),
                sponsor_one,
                U256::from(60),
            ),
            validated_with_funding(
                protocol_transaction(Account::Alice, 2, 10, 100),
                1,
                now + Duration::from_millis(2),
                sponsor_two,
                U256::from(70),
            ),
        ];
        for transaction in transactions {
            store.insert_validated(transaction, 0).unwrap();
        }

        assert_eq!(store.size().pending, 3);
        assert_eq!(store.payer_reserved(Account::Alice.address()), U256::from(40));
        assert_eq!(store.payer_reserved(sponsor_one), U256::from(60));
        assert_eq!(store.payer_reserved(sponsor_two), U256::from(70));
        assert_consistent(&store);
    }

    #[test]
    fn shared_payer_insertion_preserves_incumbent_and_removal_promotes_waiter() {
        let now = Instant::now();
        let payer = Address::repeat_byte(0x33);
        let mut store = store(0);
        store.set_payer_balance(payer, U256::from(60));
        let incumbent = validated_with_funding(
            protocol_transaction(Account::Alice, 0, 10, 100),
            1,
            now,
            payer,
            U256::from(60),
        );
        let incumbent_hash = *incumbent.hash();
        let waiter = validated_with_funding(
            protocol_transaction(Account::Bob, 0, 100, 200),
            2,
            now + Duration::from_millis(1),
            payer,
            U256::from(60),
        );
        let waiter_id = waiter.transaction.identity();
        store.insert_validated(incumbent, 0).unwrap();
        store.insert_validated(waiter, 0).unwrap();

        assert_eq!(store.size().pending, 1);
        assert!(matches!(
            store.funding_state(&waiter_id),
            Some(TransactionFundingState::Waiting(
                FundingWaitReason::InsufficientPayerBalance { .. }
            ))
        ));

        store.remove_exact(&[incumbent_hash]);
        assert_eq!(store.state(&waiter_id), Some(LaneTransactionState::Pending));
        assert_eq!(store.payer_reserved(payer), U256::from(60));
        assert_consistent(&store);
    }

    #[test]
    fn balance_increase_promotes_waiting_heads_by_stable_priority() {
        let now = Instant::now();
        let payer = Address::repeat_byte(0x44);
        let mut store = store(0);
        store.set_payer_balance(payer, U256::ZERO);
        let lower = validated_with_funding(
            protocol_transaction(Account::Alice, 0, 10, 100),
            1,
            now,
            payer,
            U256::from(50),
        );
        let lower_id = lower.transaction.identity();
        let higher = validated_with_funding(
            protocol_transaction(Account::Bob, 0, 20, 100),
            2,
            now + Duration::from_millis(1),
            payer,
            U256::from(50),
        );
        let higher_id = higher.transaction.identity();
        store.insert_validated(lower, 0).unwrap();
        store.insert_validated(higher, 0).unwrap();

        let outcome = store.set_payer_balance(payer, U256::from(50));
        assert_eq!(hashes(outcome.promoted), hashes([store.get_by_identity(&higher_id).unwrap()]));
        assert_eq!(store.state(&higher_id), Some(LaneTransactionState::Pending));
        assert!(matches!(store.state(&lower_id), Some(LaneTransactionState::Funding(_))));
        assert_eq!(outcome.funding_transitions.len(), 1);
        assert_consistent(&store);
    }

    #[test]
    fn replacement_cost_increase_atomically_reallocates_lane_funding() {
        let now = Instant::now();
        let payer = Address::repeat_byte(0x55);
        let mut store = store(0);
        store.set_payer_balance(payer, U256::from(100));
        let original = validated_with_funding(
            protocol_transaction(Account::Alice, 0, 10, 100),
            1,
            now,
            payer,
            U256::from(40),
        );
        let original_hash = *original.hash();
        let descendant = validated_with_funding(
            protocol_transaction(Account::Alice, 1, 10, 100),
            1,
            now + Duration::from_millis(1),
            payer,
            U256::from(50),
        );
        let descendant_id = descendant.transaction.identity();
        store.insert_validated(original, 0).unwrap();
        store.insert_validated(descendant, 0).unwrap();

        let replacement = validated_with_funding(
            protocol_transaction(Account::Alice, 0, 12, 120),
            1,
            now + Duration::from_millis(2),
            payer,
            U256::from(80),
        );
        let replacement_id = replacement.transaction.identity();
        let outcome = store.insert_validated(replacement, 0).unwrap();

        assert_eq!(outcome.replaced.map(|transaction| *transaction.hash()), Some(original_hash));
        assert_eq!(store.state(&replacement_id), Some(LaneTransactionState::Pending));
        assert!(matches!(
            store.state(&descendant_id),
            Some(LaneTransactionState::Funding(FundingWaitReason::InsufficientPayerBalance { .. }))
        ));
        assert_eq!(store.payer_reserved(payer), U256::from(80));
        assert_consistent(&store);
    }

    #[test]
    fn balance_decrease_demotes_selected_reservation_and_lane_suffix() {
        let now = Instant::now();
        let constrained = Address::repeat_byte(0x66);
        let suffix_payer = Address::repeat_byte(0x77);
        let mut store = store(0);
        store.set_payer_balance(constrained, U256::from(100));
        store.set_payer_balance(suffix_payer, U256::from(40));
        let alice_head = validated_with_funding(
            protocol_transaction(Account::Alice, 0, 5, 100),
            1,
            now,
            constrained,
            U256::from(60),
        );
        let alice_head_id = alice_head.transaction.identity();
        let alice_suffix = validated_with_funding(
            protocol_transaction(Account::Alice, 1, 50, 100),
            1,
            now + Duration::from_millis(1),
            suffix_payer,
            U256::from(40),
        );
        let alice_suffix_id = alice_suffix.transaction.identity();
        let bob_head = validated_with_funding(
            protocol_transaction(Account::Bob, 0, 20, 100),
            2,
            now + Duration::from_millis(2),
            constrained,
            U256::from(40),
        );
        let bob_head_id = bob_head.transaction.identity();
        for transaction in [alice_head, alice_suffix, bob_head] {
            store.insert_validated(transaction, 0).unwrap();
        }

        let decreased = store.set_payer_balance(constrained, U256::from(40));
        assert_eq!(decreased.demoted.len(), 2);
        assert!(matches!(store.state(&alice_head_id), Some(LaneTransactionState::Funding(_))));
        assert!(matches!(
            store.state(&alice_suffix_id),
            Some(LaneTransactionState::Queued(LaneGap::BlockedByFunding { ancestor: 0 }))
        ));
        assert_eq!(store.state(&bob_head_id), Some(LaneTransactionState::Pending));
        assert_eq!(store.payer_reserved(suffix_payer), U256::ZERO);

        let increased = store.set_payer_balance(constrained, U256::from(100));
        assert_eq!(increased.promoted.len(), 2);
        assert_eq!(store.size().pending, 3);
        assert_consistent(&store);
    }

    #[test]
    fn replay_reservations_compete_independently() {
        let signer = PrivateKeySigner::random();
        let now = Instant::now();
        let payer = Address::repeat_byte(0x88);
        let mut store = store(0);
        store.set_payer_balance(payer, U256::ZERO);
        let lower = validated_with_funding(
            sidecar_transaction(&signer, Eip8130Constants::NONCE_KEY_MAX, 0, 1, 10, 100),
            1,
            now,
            payer,
            U256::from(30),
        );
        let lower_id = lower.transaction.identity();
        let higher = validated_with_funding(
            sidecar_transaction(&signer, Eip8130Constants::NONCE_KEY_MAX, 0, 2, 20, 100),
            1,
            now + Duration::from_millis(1),
            payer,
            U256::from(30),
        );
        let higher_id = higher.transaction.identity();
        store.insert_validated(lower, 0).unwrap();
        store.insert_validated(higher, 0).unwrap();

        store.set_payer_balance(payer, U256::from(30));
        assert_eq!(store.state(&higher_id), Some(LaneTransactionState::Pending));
        assert!(matches!(store.state(&lower_id), Some(LaneTransactionState::Funding(_))));
        assert_consistent(&store);
    }

    #[test]
    fn randomized_funding_operations_preserve_reservation_invariants() {
        let signer = PrivateKeySigner::random();
        let now = Instant::now();
        let payers =
            [Address::repeat_byte(0x91), Address::repeat_byte(0x92), Address::repeat_byte(0x93)];
        let mut store = store(0);
        for payer in payers {
            store.set_payer_balance(payer, U256::from(150));
        }
        let mut seed = 0xa076_1d64_78bd_642f_u64;

        for step in 0..350_u64 {
            seed = seed.rotate_left(17).wrapping_mul(0xe703_7ed1_a0b4_28db).wrapping_add(step);
            match seed % 10 {
                0..=4 => {
                    let nonce = (seed >> 8) % 6;
                    let fee = 1_000 + u128::from(step) * 20;
                    let transaction = match (seed >> 16) % 4 {
                        0 => protocol_transaction(Account::Alice, nonce, fee / 10, fee),
                        1 => protocol_transaction(Account::Bob, nonce, fee / 10, fee),
                        2 => sidecar_transaction(
                            &signer,
                            U256::from(((seed >> 24) % 2) + 1),
                            nonce,
                            0,
                            fee / 10,
                            fee,
                        ),
                        _ => sidecar_transaction(
                            &signer,
                            Eip8130Constants::NONCE_KEY_MAX,
                            0,
                            nonce + 1,
                            fee / 10,
                            fee,
                        ),
                    };
                    let payer = payers[((seed >> 32) as usize) % payers.len()];
                    let cost = U256::from(((seed >> 40) % 90) + 1);
                    let _ = store.insert_validated(
                        validated_with_funding(
                            transaction,
                            seed & 3,
                            now + Duration::from_millis(step),
                            payer,
                            cost,
                        ),
                        0,
                    );
                }
                5 => {
                    if let Some(transaction) =
                        store.all_transactions().get((seed as usize) % store.len().max(1))
                    {
                        store.remove_exact(&[*transaction.hash()]);
                    }
                }
                6 => {
                    if let Some(transaction) =
                        store.all_transactions().get((seed as usize) % store.len().max(1))
                    {
                        store.remove_with_descendants(&[*transaction.hash()]);
                    }
                }
                7 => {
                    let payer = payers[((seed >> 20) as usize) % payers.len()];
                    store.set_payer_balance(payer, U256::from((seed >> 28) % 220));
                }
                8 => {
                    let payer = payers[((seed >> 20) as usize) % payers.len()];
                    store.remove_payer_balance(payer);
                }
                _ => {
                    let payer = payers[((seed >> 20) as usize) % payers.len()];
                    if store.payer_balance(payer).is_none() {
                        store.set_payer_balance(payer, U256::from((seed >> 28) % 220));
                    }
                }
            }

            assert_consistent(&store);
        }
    }

    #[test]
    fn randomized_mixed_operations_preserve_indexes_and_lane_ordering() {
        let signer = PrivateKeySigner::random();
        let now = Instant::now();
        let mut store = store(0);
        let mut seed = 0xd1b5_4a32_d192_ed03_u64;

        for step in 0..400_u64 {
            seed = seed.rotate_left(13).wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(step);
            match seed % 8 {
                0..=3 => {
                    let nonce = (seed >> 9) % 8;
                    let fee = 1_000 + u128::from(step) * 25;
                    let transaction = match (seed >> 5) % 4 {
                        0 => protocol_transaction(Account::Alice, nonce, fee / 10, fee),
                        1 => protocol_transaction(Account::Bob, nonce, fee / 10, fee),
                        2 => sidecar_transaction(&signer, U256::from(11), nonce, 0, fee / 10, fee),
                        _ => sidecar_transaction(
                            &signer,
                            Eip8130Constants::NONCE_KEY_MAX,
                            0,
                            nonce + 1,
                            fee / 10,
                            fee,
                        ),
                    };
                    let _ = store.insert_validated(
                        validated(transaction, seed & 3, now + Duration::from_millis(step)),
                        0,
                    );
                }
                4 => {
                    if let Some(transaction) =
                        store.all_transactions().get((seed as usize) % store.len().max(1))
                    {
                        store.remove_exact(&[*transaction.hash()]);
                    }
                }
                5 => {
                    if let Some(transaction) =
                        store.all_transactions().get((seed as usize) % store.len().max(1))
                    {
                        store.remove_with_descendants(&[*transaction.hash()]);
                    }
                }
                6 => {
                    if let Some(transaction) =
                        store.all_transactions().get((seed as usize) % store.len().max(1))
                    {
                        store.commit(&[transaction.transaction.identity()]);
                    }
                }
                _ => {
                    store.set_base_fee((seed >> 20) % 2_000);
                }
            }

            assert_consistent(&store);
            let pending: HashSet<_> = store
                .classified_transactions()
                .filter(|(state, _)| *state == LaneTransactionState::Pending)
                .map(|(_, transaction)| *transaction.hash())
                .collect();
            let best = store.best_transactions(BaseOrdering::coinbase_tip(), store.base_fee());
            let mut last_nonce = HashMap::<BaseTransactionLane, u64>::default();
            for transaction in best {
                assert!(pending.contains(transaction.hash()));
                if let BaseTransactionIdentity::Nonce { lane, nonce } =
                    transaction.transaction.identity()
                    && let Some(previous) = last_nonce.insert(lane, nonce)
                {
                    assert_eq!(nonce, previous + 1);
                }
            }
        }
    }

    #[test]
    fn lifecycle_batches_cover_protocol_channel_and_replay_promotions() {
        let signer = PrivateKeySigner::random();
        let now = Instant::now();
        let mut store = store(50);
        let protocol_gap = validated(protocol_transaction(Account::Alice, 1, 10, 100), 1, now);
        let protocol_gap_hash = *protocol_gap.hash();
        let channel = validated(sidecar_transaction(&signer, U256::from(7), 0, 0, 20, 100), 1, now);
        let replay = validated(
            sidecar_transaction(&signer, Eip8130Constants::NONCE_KEY_MAX, 0, 1, 20, 40),
            1,
            now,
        );

        for transaction in [protocol_gap, channel, replay] {
            let outcome = store.insert_validated(transaction, 0).unwrap();
            assert_eq!(outcome.transitions.cause, LaneTransitionCause::Insert);
            assert_eq!(outcome.transitions.transitions.len(), 1);
            let transition = &outcome.transitions.transitions[0];
            assert_eq!(transition.previous_state, None);
            assert!(transition.current_state.is_some());
            assert_eq!(transition.terminal, None);
        }

        let head = validated(protocol_transaction(Account::Alice, 0, 30, 100), 1, now);
        let head_hash = *head.hash();
        let outcome = store.insert_validated(head, 0).unwrap();
        assert_eq!(outcome.transitions.transitions.len(), 2);
        assert!(outcome.transitions.transitions.iter().any(|transition| {
            *transition.transaction.hash() == head_hash
                && transition.previous_state.is_none()
                && transition.current_state == Some(LaneTransactionState::Pending)
        }));
        assert!(outcome.transitions.transitions.iter().any(|transition| {
            *transition.transaction.hash() == protocol_gap_hash
                && matches!(transition.previous_state, Some(LaneTransactionState::Queued(_)))
                && transition.current_state == Some(LaneTransactionState::Pending)
        }));
    }

    #[test]
    fn replacement_funding_and_fee_updates_are_coherent_batches() {
        let now = Instant::now();
        let payer = Address::repeat_byte(0xa1);
        let mut store = store(0);
        store.set_payer_balance(payer, U256::from(100));
        let original = validated_with_funding(
            protocol_transaction(Account::Alice, 0, 10, 100),
            1,
            now,
            payer,
            U256::from(40),
        );
        let original_hash = *original.hash();
        store.insert_validated(original, 0).unwrap();
        let replacement = validated_with_funding(
            protocol_transaction(Account::Alice, 0, 12, 120),
            1,
            now + Duration::from_millis(1),
            payer,
            U256::from(80),
        );
        let replacement_hash = *replacement.hash();
        let replaced = store.insert_validated(replacement, 0).unwrap();

        assert!(replaced.transitions.transitions.iter().any(|transition| {
            *transition.transaction.hash() == original_hash
                && transition.current_state.is_none()
                && transition.terminal == Some(LaneTerminalEvent::Replaced { by: replacement_hash })
        }));
        assert!(replaced.transitions.transitions.iter().any(|transition| {
            *transition.transaction.hash() == replacement_hash
                && transition.previous_state.is_none()
                && transition.current_state == Some(LaneTransactionState::Pending)
        }));

        let decreased = store.set_payer_balance(payer, U256::from(50));
        assert_eq!(decreased.transitions.cause, LaneTransitionCause::PayerBalance);
        assert_eq!(decreased.demoted.len(), 1);
        assert_eq!(decreased.funding_transitions.len(), 1);
        let increased = store.set_payer_balance(payer, U256::from(100));
        assert_eq!(increased.promoted.len(), 1);

        let raised = store.set_base_fee(121);
        assert_eq!(raised.transitions.cause, LaneTransitionCause::BaseFee);
        assert_eq!(raised.demoted.len(), 1);
        let lowered = store.set_base_fee(120);
        assert_eq!(lowered.promoted.len(), 1);
    }

    #[test]
    fn cross_envelope_replacement_has_one_terminal_and_one_admission() {
        let now = Instant::now();
        let mut store = store(0);
        let legacy = validated(legacy_protocol_transaction(Account::Alice, 0, 100), 1, now);
        let legacy_hash = *legacy.hash();
        store.insert_validated(legacy, 0).unwrap();
        let dynamic = validated(protocol_transaction(Account::Alice, 0, 12, 120), 1, now);
        let dynamic_hash = *dynamic.hash();

        let outcome = store.insert_validated(dynamic, 0).unwrap();

        assert_eq!(outcome.transitions.transitions.len(), 2);
        assert!(outcome.transitions.transitions.iter().any(|transition| {
            *transition.transaction.hash() == legacy_hash
                && transition.terminal == Some(LaneTerminalEvent::Replaced { by: dynamic_hash })
        }));
        assert!(outcome.transitions.transitions.iter().any(|transition| {
            *transition.transaction.hash() == dynamic_hash
                && transition.previous_state.is_none()
                && transition.current_state == Some(LaneTransactionState::Pending)
        }));
    }

    #[test]
    fn eviction_expiry_and_mining_have_single_terminal_transitions() {
        let now = Instant::now();
        let mut config = PoolConfig { max_account_slots: usize::MAX, ..PoolConfig::default() };
        config.pending_limit.max_txs = 1;
        let mut store = LaneTransactionStore::new(config, 0);
        store.set_payer_balance(Address::ZERO, U256::ZERO);
        let lower = validated(protocol_transaction(Account::Alice, 0, 10, 100), 1, now);
        let lower_hash = *lower.hash();
        let higher = validated(protocol_transaction(Account::Bob, 0, 20, 100), 2, now);
        let higher_hash = *higher.hash();
        store.insert_validated(lower, 0).unwrap();
        store.insert_validated(higher, 0).unwrap();

        let evicted = store.enforce_limits_with_outcome();
        assert_eq!(hashes(evicted.removed), vec![lower_hash]);
        assert_eq!(evicted.transitions.transitions.len(), 1);
        assert_eq!(evicted.transitions.transitions[0].terminal, Some(LaneTerminalEvent::Evicted));

        let expired = store.remove_expired(&[higher_hash]);
        assert_eq!(expired.transitions.transitions[0].terminal, Some(LaneTerminalEvent::Expired));

        let mined = validated(protocol_transaction(Account::Alice, 1, 30, 100), 1, now);
        let mined_hash = *mined.hash();
        store.insert_validated(mined, 1).unwrap();
        let block_hash = B256::repeat_byte(0xb1);
        let mined = store.prune_mined(&[mined_hash], block_hash);
        assert_eq!(
            mined.transitions.transitions[0].terminal,
            Some(LaneTerminalEvent::Mined { block_hash })
        );

        let replay = validated(
            sidecar_transaction(
                &PrivateKeySigner::random(),
                Eip8130Constants::NONCE_KEY_MAX,
                0,
                1,
                10,
                100,
            ),
            3,
            now,
        );
        let replay_identity = replay.transaction.identity();
        store.insert_validated(replay, 0).unwrap();
        let committed = store.commit(&[replay_identity]);
        assert_eq!(
            committed.transitions.transitions[0].terminal,
            Some(LaneTerminalEvent::Committed)
        );
    }

    #[tokio::test]
    async fn event_hub_matches_reth_events_and_survives_backpressure() {
        let now = Instant::now();
        let mut store = store(0);
        let hub = LaneEventHub::new(1);
        let mut all = hub.all_transactions_event_listener();
        let mut pending_all = hub.pending_transactions_listener_for(TransactionListenerKind::All);
        let mut pending_propagate =
            hub.pending_transactions_listener_for(TransactionListenerKind::PropagateOnly);
        let mut new_all = hub.new_transactions_listener_for(TransactionListenerKind::All);
        let mut transaction = validated(protocol_transaction(Account::Alice, 0, 10, 100), 1, now);
        transaction.propagate = false;
        let hash = *transaction.hash();
        let mut by_hash = hub.transaction_event_listener(hash);

        let inserted = store.insert_validated(transaction, 0).unwrap();
        hub.publish(&inserted.transitions);
        assert_eq!(by_hash.next().await, Some(TransactionEvent::Pending));
        assert_eq!(pending_all.recv().await, Some(hash));
        assert!(pending_propagate.try_recv().is_err());
        assert_eq!(new_all.recv().await.unwrap().subpool, SubPool::Pending);

        let demoted = store.set_base_fee(101);
        hub.publish(&demoted.transitions);
        assert!(new_all.try_recv().is_err());
        assert!(
            matches!(all.next().await, Some(FullTransactionEvent::Pending(event)) if event == hash)
        );
        let promoted = store.set_base_fee(100);
        hub.publish(&promoted.transitions);
        assert_eq!(new_all.recv().await.unwrap().subpool, SubPool::Pending);
        assert!(
            matches!(all.next().await, Some(FullTransactionEvent::Pending(event)) if event == hash)
        );

        let block_hash = B256::repeat_byte(0xc1);
        let mined = store.prune_mined(&[hash], block_hash);
        hub.publish(&mined.transitions);
        hub.publish(&mined.transitions);
        assert!(matches!(
            all.next().await,
            Some(FullTransactionEvent::Mined { tx_hash, block_hash: event_block })
                if tx_hash == hash && event_block == block_hash
        ));
        assert!(tokio::time::timeout(Duration::from_millis(10), all.next()).await.is_err());
        assert_eq!(by_hash.next().await, Some(TransactionEvent::Queued));
        assert_eq!(by_hash.next().await, Some(TransactionEvent::Pending));
        assert_eq!(by_hash.next().await, Some(TransactionEvent::Mined(block_hash)));
        assert_eq!(by_hash.next().await, None);
    }

    #[test]
    fn canonical_update_rebalances_fee_nonce_and_balance_once() {
        let now = Instant::now();
        let mut store = store(0);
        let first = validated(protocol_transaction(Account::Alice, 0, 10, 100), 1, now);
        let second = validated(protocol_transaction(Account::Alice, 1, 9, 100), 1, now);
        let first_hash = *first.hash();
        let second_identity = second.transaction.identity();
        store.insert_validated(first, 0).unwrap();
        store.insert_validated(second, 0).unwrap();

        let block_hash = B256::repeat_byte(0xd1);
        let outcome = store.apply_canonical_update(
            101,
            &[first_hash],
            block_hash,
            &[LaneCanonicalAccountUpdate {
                address: Account::Alice.address(),
                nonce: 1,
                balance: U256::ZERO,
            }],
            &[],
            &[],
        );
        assert_eq!(hashes(outcome.removed), vec![first_hash]);
        assert_eq!(store.base_fee(), 101);
        assert_eq!(
            store.lane_cursor(&BaseTransactionLane::Protocol { sender: Account::Alice.address() }),
            Some(1)
        );
        assert_eq!(store.state(&second_identity), Some(LaneTransactionState::BaseFee));
        assert_eq!(outcome.transitions.cause, LaneTransitionCause::Mining);
        assert!(outcome.transitions.transitions.iter().any(|transition| {
            transition.terminal == Some(LaneTerminalEvent::Mined { block_hash })
        }));
    }

    #[test]
    fn minimum_fee_and_account_capacity_allow_head_and_gap_fill() {
        let config = PoolConfig {
            minimal_protocol_basefee: 100,
            max_account_slots: 1,
            ..PoolConfig::default()
        };
        let mut store = LaneTransactionStore::new(config, 0);
        let now = Instant::now();
        let under = validated(protocol_transaction(Account::Alice, 0, 1, 99), 1, now);
        assert!(matches!(
            store.insert_validated(under, 0).unwrap_err().kind,
            PoolErrorKind::FeeCapBelowMinimumProtocolFeeCap(99)
        ));

        store
            .insert_validated(validated(protocol_transaction(Account::Alice, 2, 1, 100), 1, now), 0)
            .unwrap();
        store
            .insert_validated(validated(protocol_transaction(Account::Alice, 1, 1, 100), 1, now), 0)
            .unwrap();
        store
            .insert_validated(validated(protocol_transaction(Account::Alice, 0, 1, 100), 1, now), 0)
            .unwrap();
        let append = validated(protocol_transaction(Account::Alice, 3, 1, 100), 1, now);
        assert!(matches!(
            store.insert_validated(append, 0).unwrap_err().kind,
            PoolErrorKind::SpammerExceededCapacity(_)
        ));
    }

    #[test]
    fn eviction_preserves_lane_prefix_and_uses_local_only_as_fallback() {
        let config = PoolConfig {
            pending_limit: reth_transaction_pool::SubPoolLimit { max_txs: 2, max_size: usize::MAX },
            minimal_protocol_basefee: 0,
            ..PoolConfig::default()
        };
        let now = Instant::now();
        let mut store = LaneTransactionStore::new(config, 0);
        store.set_payer_balance(Address::ZERO, U256::ZERO);
        let first = validated(protocol_transaction(Account::Alice, 0, 10, 100), 1, now);
        let second = validated(protocol_transaction(Account::Alice, 1, 1, 100), 1, now);
        let second_hash = *second.hash();
        let mut local = validated(protocol_transaction(Account::Bob, 0, 0, 100), 2, now);
        local.origin = TransactionOrigin::Local;
        store.insert_validated(first, 0).unwrap();
        store.insert_validated(second, 0).unwrap();
        store.insert_validated(local, 0).unwrap();
        assert_eq!(store.size().pending, 3);

        let evicted = store.enforce_limits_with_outcome();
        assert_eq!(hashes(evicted.removed), vec![second_hash]);
        assert_eq!(store.pending_transactions().len(), 2);
        assert!(store.protocol_transaction(Account::Alice.address(), 0).is_some());
    }

    #[test]
    fn pruning_max_nonce_exhausts_lane() {
        let now = Instant::now();
        let mut store = store(0);
        let transaction =
            validated(protocol_transaction(Account::Alice, u64::MAX, 10, 100), 1, now);
        let hash = *transaction.hash();
        store.insert_validated(transaction, u64::MAX).unwrap();
        store.prune_speculative(&[hash]);
        let replacement =
            validated(protocol_transaction(Account::Alice, u64::MAX, 20, 200), 1, now);
        assert!(matches!(
            store.insert_validated(replacement, u64::MAX).unwrap_err().kind,
            PoolErrorKind::InvalidTransaction(_)
        ));
    }

    #[test]
    fn size_uses_transaction_memory_footprint() {
        let now = Instant::now();
        let mut store = store(0);
        let transaction = validated(protocol_transaction(Account::Alice, 0, 10, 100), 1, now);
        let expected = transaction.transaction.size();
        store.insert_validated(transaction, 0).unwrap();
        assert_eq!(store.size().pending_size, expected);
    }

    #[test]
    fn configured_timestamp_ordering_drives_eviction() {
        let config = PoolConfig {
            pending_limit: reth_transaction_pool::SubPoolLimit { max_txs: 1, max_size: usize::MAX },
            minimal_protocol_basefee: 0,
            ..PoolConfig::default()
        };
        let mut store = LaneTransactionStore::new(config, 0);
        store.set_payer_balance(Address::ZERO, U256::ZERO);
        let old = protocol_transaction(Account::Alice, 0, 1, 10);
        let old_len = old.encoded_length();
        let old = BasePooledTransaction::new_with_received_at(old.into_consensus(), old_len, 1);
        let new = protocol_transaction(Account::Bob, 0, 100, 1_000);
        let new_len = new.encoded_length();
        let new = BasePooledTransaction::new_with_received_at(new.into_consensus(), new_len, 2);
        let old = validated(old, 1, Instant::now());
        let new = validated(new, 2, Instant::now());
        let new_hash = *new.hash();
        store.insert_validated(old, 0).unwrap();
        store.insert_validated(new, 0).unwrap();

        let evicted = store.enforce_limits_with_ordering(&BaseOrdering::timestamp());
        assert_eq!(hashes(evicted.removed), vec![new_hash]);
    }

    #[test]
    fn configured_effective_tip_drives_eviction() {
        let config = PoolConfig {
            pending_limit: reth_transaction_pool::SubPoolLimit { max_txs: 1, max_size: usize::MAX },
            minimal_protocol_basefee: 0,
            ..PoolConfig::default()
        };
        let mut store = LaneTransactionStore::new(config, 90);
        store.set_payer_balance(Address::ZERO, U256::ZERO);
        let high_effective =
            validated(protocol_transaction(Account::Alice, 0, 100, 100), 1, Instant::now());
        let low_effective =
            validated(protocol_transaction(Account::Bob, 0, 5, 200), 2, Instant::now());
        let low_hash = *low_effective.hash();
        store.insert_validated(high_effective, 0).unwrap();
        store.insert_validated(low_effective, 0).unwrap();

        let evicted = store.enforce_limits_with_ordering(&BaseOrdering::coinbase_tip());
        assert_eq!(hashes(evicted.removed), vec![low_hash]);
    }

    #[test]
    fn journal_closes_transitively_across_p_and_q_lanes() {
        let now = Instant::now();
        let payer_p = Address::repeat_byte(0xf1);
        let payer_q = Address::repeat_byte(0xf2);
        let mut store = store(0);
        store.set_payer_balance(payer_p, U256::from(10));
        store.set_payer_balance(payer_q, U256::from(10));

        let a0 = validated_with_funding(
            protocol_transaction(Account::Alice, 0, 30, 100),
            1,
            now,
            payer_p,
            U256::from(10),
        );
        let a1 = validated_with_funding(
            protocol_transaction(Account::Alice, 1, 20, 100),
            1,
            now,
            payer_q,
            U256::from(10),
        );
        let b0 = validated_with_funding(
            protocol_transaction(Account::Bob, 0, 10, 100),
            2,
            now,
            payer_q,
            U256::from(10),
        );
        let a1_hash = *a1.hash();
        let b0_hash = *b0.hash();
        store.insert_validated(a0, 0).unwrap();
        store.insert_validated(a1, 0).unwrap();
        store.insert_validated(b0, 0).unwrap();
        assert_eq!(
            store.state(&store.get_by_hash(&b0_hash).unwrap().transaction.identity()),
            Some(LaneTransactionState::Funding(FundingWaitReason::InsufficientPayerBalance {
                payer: payer_q,
                required: U256::from(10),
                available: U256::ZERO,
            }))
        );

        let replacement = validated_with_funding(
            protocol_transaction(Account::Alice, 0, 40, 120),
            1,
            now,
            payer_p,
            U256::from(20),
        );
        store.preflight_insert(&replacement, 0).unwrap();
        let before_size = store.size();
        let mut journal = store.mutation_journal();
        store.journal_insertion(&mut journal, &replacement);
        let replaced = store.protocol_transaction(Account::Alice.address(), 0).unwrap();
        let replaced_hash = *replaced.hash();
        store.insert_preflighted_journaled(replacement, 0, &journal);
        let after_size = store.size_after_journal(before_size, &journal);
        let transitions = store.transitions_since_journal(
            journal,
            LaneTransitionCause::Insert,
            vec![LaneTerminalTransition {
                transaction: replaced,
                terminal: LaneTerminalEvent::Replaced {
                    by: *store.protocol_transaction(Account::Alice.address(), 0).unwrap().hash(),
                },
            }],
        );
        assert!(transitions.transitions.iter().any(|transition| {
            *transition.transaction.hash() == a1_hash
                && transition.previous_state == Some(LaneTransactionState::Pending)
                && matches!(transition.current_state, Some(LaneTransactionState::Queued(_)))
        }));
        assert!(transitions.transitions.iter().any(|transition| {
            *transition.transaction.hash() == b0_hash
                && matches!(transition.previous_state, Some(LaneTransactionState::Funding(_)))
                && transition.current_state == Some(LaneTransactionState::Pending)
        }));
        assert_eq!(after_size.pending, 1);
        assert_eq!(after_size.queued, 2);
        assert!(!store.contains_hash(&replaced_hash));
        assert_eq!(store.size().pending, 1);
        assert_eq!(store.size().queued, 2);
        assert_consistent(&store);
    }

    #[test]
    fn payer_update_does_not_scan_unrelated_lanes_or_payers() {
        let signer = PrivateKeySigner::random();
        let now = Instant::now();
        let mut store = store(0);
        let mut first = None;
        for index in 1..=64u64 {
            let payer = Address::from_word(B256::from(U256::from(index)));
            store.set_payer_balance(payer, U256::from(10));
            let transaction = validated_with_funding(
                sidecar_transaction(&signer, U256::from(index), 0, 0, 10, 100),
                index,
                now,
                payer,
                U256::from(10),
            );
            if index == 1 {
                first = Some((payer, *transaction.hash()));
            }
            store.insert_validated(transaction, 0).unwrap();
        }
        let (payer, first_hash) = first.unwrap();
        store.reset_full_classification_passes();
        store.reset_incremental_scan_counts();

        let outcome = store.set_payer_balance(payer, U256::ZERO);
        assert!(
            outcome
                .transitions
                .transitions
                .iter()
                .all(|transition| { *transition.transaction.hash() == first_hash })
        );
        assert_eq!(store.full_classification_passes(), 0);
        let (lane_scans, payer_scans) = store.incremental_scan_counts();
        assert!(lane_scans <= 4, "unrelated lane scans: {lane_scans}");
        assert!(payer_scans <= 4, "unrelated payer scans: {payer_scans}");
        assert_consistent(&store);
    }
}
