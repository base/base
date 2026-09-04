//! Generalized lane-aware storage for validated pool transactions.

use std::{
    collections::{BTreeMap, BinaryHeap, HashSet},
    sync::Arc,
};

use alloy_primitives::{
    Address, B256, TxHash,
    map::{B256Map, HashMap},
};
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_transaction_pool::{
    AddedTransactionOutcome, BestTransactions, PoolConfig, PoolResult, SubPool,
    TransactionOrdering, ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolError, PoolErrorKind},
    pool::{AddedTransactionState, QueuedReason},
};

use crate::{BasePooledTx, BaseTransactionIdentity, BaseTransactionLane, BestTransactionPriority};

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
}

/// Current executable-state classification of a stored transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneTransactionState {
    /// The transaction and all of its dependencies are executable.
    Pending,
    /// The transaction is next in its lane but its fee cap is below the base fee.
    BaseFee,
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
}

/// State changes caused by updating the store's base fee.
#[derive(Debug)]
pub struct LaneFeeUpdateOutcome<T: BasePooledTx> {
    /// Transactions newly promoted to pending.
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Transactions moved out of pending.
    pub demoted: Vec<(Arc<ValidPoolTransaction<T>>, SubPool)>,
}

/// Result of committing canonical Base transaction identities.
#[derive(Debug)]
pub struct LaneCommitOutcome<T: BasePooledTx> {
    /// Pool transactions removed by the commit.
    pub removed: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Remaining transactions newly promoted to pending.
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
}

#[derive(Debug)]
struct NonceLane<T: BasePooledTx> {
    cursor: u64,
    transactions: BTreeMap<u64, Arc<ValidPoolTransaction<T>>>,
}

impl<T: BasePooledTx> Default for NonceLane<T> {
    fn default() -> Self {
        Self { cursor: 0, transactions: BTreeMap::new() }
    }
}

impl<T: BasePooledTx> NonceLane<T> {
    fn live(&self) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.transactions.range(self.cursor..).map(|(_, transaction)| transaction)
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
    config: PoolConfig,
    base_fee: u64,
}

impl<T: BasePooledTx> LaneTransactionStore<T> {
    /// Creates an empty store using Reth pool limits and replacement policy.
    pub fn new(config: PoolConfig, base_fee: u64) -> Self {
        Self {
            lanes: HashMap::default(),
            replays: B256Map::default(),
            identities: HashMap::default(),
            hashes: B256Map::default(),
            senders: HashMap::default(),
            config,
            base_fee,
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
            BaseTransactionIdentity::Replay { replay_id } => {
                self.replays.get(&replay_id).map(|transaction| {
                    if transaction.transaction.max_fee_per_gas() < u128::from(self.base_fee) {
                        LaneTransactionState::BaseFee
                    } else {
                        LaneTransactionState::Pending
                    }
                })
            }
            BaseTransactionIdentity::Nonce { lane, .. } => {
                self.lanes.get(&lane)?.classified(self.base_fee).into_iter().find_map(
                    |(state, transaction)| {
                        (transaction.transaction.identity() == *identity).then_some(state)
                    },
                )
            }
        }
    }

    /// Returns aggregate counts and encoded sizes for each state.
    pub fn size(&self) -> LaneStoreSize {
        let mut size = LaneStoreSize::default();
        for (state, transaction) in self.classified_transactions() {
            size.total += 1;
            match state {
                LaneTransactionState::Pending => {
                    size.pending += 1;
                    size.pending_size += transaction.encoded_length();
                }
                LaneTransactionState::BaseFee => {
                    size.basefee += 1;
                    size.basefee_size += transaction.encoded_length();
                }
                LaneTransactionState::Queued(_) => {
                    size.queued += 1;
                    size.queued_size += transaction.encoded_length();
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
            .filter(|(state, _)| matches!(state, LaneTransactionState::Queued(_)))
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
        let hash = *transaction.hash();
        if self.hashes.contains_key(&hash) {
            return Err(PoolError::new(hash, PoolErrorKind::AlreadyImported));
        }
        let identity = transaction.transaction.identity();
        if let BaseTransactionIdentity::Nonce { lane, nonce } = identity {
            let current_cursor = self.lanes.get(&lane).map_or(lane_cursor, |stored| stored.cursor);
            if nonce < current_cursor {
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

        let transaction = Arc::new(transaction);
        let replaced = self.identities.get(&identity).cloned();
        if let Some(existing) = &replaced
            && existing.is_underpriced(&transaction, &self.config.price_bumps)
        {
            return Err(PoolError::new(hash, PoolErrorKind::ReplacementUnderpriced));
        }
        self.ensure_sender_capacity(&transaction, replaced.is_some())?;

        let before = self.states_by_hash();
        if let Some(existing) = &replaced {
            self.remove_indexed(existing.transaction.identity());
        }
        match identity {
            BaseTransactionIdentity::Nonce { lane, nonce } => {
                self.lanes
                    .entry(lane)
                    .or_insert_with(|| NonceLane {
                        cursor: lane_cursor,
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

        let state = self.state(&identity).expect("inserted transaction is classified");
        let promoted = self
            .classified_transactions()
            .filter(|(candidate, tx)| {
                *candidate == LaneTransactionState::Pending
                    && *tx.hash() != hash
                    && before
                        .get(tx.hash())
                        .is_some_and(|previous| *previous != LaneTransactionState::Pending)
            })
            .map(|(_, transaction)| Arc::clone(transaction))
            .collect();
        Ok(LaneInsertOutcome {
            outcome: AddedTransactionOutcome { hash, state: Self::added_state(state) },
            replaced,
            promoted,
        })
    }

    /// Sets a lane cursor, removing transactions made stale by a forward move.
    pub fn set_lane_cursor(
        &mut self,
        lane: BaseTransactionLane,
        cursor: u64,
    ) -> LaneUpdateOutcome<T> {
        let before = self.states_by_hash();
        let stale = {
            let lane = self.lanes.entry(lane).or_default();
            lane.cursor = cursor;
            lane.transactions
                .range(..cursor)
                .map(|(_, transaction)| *transaction.hash())
                .collect::<Vec<_>>()
        };
        let removed = self.remove_exact(&stale);
        let (promoted, demoted) = self.pending_changes(&before);
        LaneUpdateOutcome { removed, promoted, demoted }
    }

    /// Updates the base fee and reports transactions entering or leaving pending.
    pub fn set_base_fee(&mut self, base_fee: u64) -> LaneFeeUpdateOutcome<T> {
        let before = self.states_by_hash();
        self.base_fee = base_fee;
        let (promoted, demoted) = self.pending_changes(&before);
        LaneFeeUpdateOutcome { promoted, demoted }
    }

    /// Removes exact hashes without advancing lane cursors.
    pub fn remove_exact(&mut self, hashes: &[TxHash]) -> Vec<Arc<ValidPoolTransaction<T>>> {
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

    /// Removes exact hashes and every nonce descendant in the same lane.
    ///
    /// Replay entries have no descendants and are removed independently.
    pub fn remove_with_descendants(
        &mut self,
        hashes: &[TxHash],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
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
        identities.into_iter().filter_map(|identity| self.remove_indexed(identity)).collect()
    }

    /// Prunes known mined hashes while preserving executable descendants.
    ///
    /// A sequential cursor advances only when the pruned transaction is its current head.
    pub fn prune(&mut self, hashes: &[TxHash]) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut identities: Vec<_> = hashes
            .iter()
            .filter_map(|hash| self.hashes.get(hash))
            .map(|transaction| transaction.transaction.identity())
            .collect();
        identities.sort_unstable_by_key(|identity| Self::identity_sort_key(*identity));
        let mut removed = Vec::new();
        for identity in identities {
            if let BaseTransactionIdentity::Nonce { lane, nonce } = identity
                && self.lanes.get(&lane).is_some_and(|stored| stored.cursor == nonce)
            {
                self.lanes.get_mut(&lane).expect("lane exists").cursor = nonce.saturating_add(1);
            }
            if let Some(transaction) = self.remove_indexed(identity) {
                removed.push(transaction);
            }
        }
        removed
    }

    /// Commits canonical identities, including identities not currently present by hash.
    ///
    /// Committing a nonce identity advances its exact lane and removes all entries through that
    /// nonce. Committing a replay identity removes only that replay entry.
    pub fn commit(&mut self, identities: &[BaseTransactionIdentity]) -> LaneCommitOutcome<T> {
        let before = self.states_by_hash();
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
                    lane.cursor = lane.cursor.max(nonce.saturating_add(1));
                    removed.extend(
                        committed.into_iter().filter_map(|stored| self.remove_indexed(stored)),
                    );
                }
            }
        }
        let (promoted, _) = self.pending_changes(&before);
        LaneCommitOutcome { removed, promoted }
    }

    /// Removes every transaction belonging to a physical sender.
    pub fn remove_by_sender(&mut self, sender: Address) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let hashes =
            self.senders.get(&sender).cloned().unwrap_or_default().into_iter().collect::<Vec<_>>();
        self.remove_exact(&hashes)
    }

    /// Evicts non-local transactions until all configured Reth subpool limits hold.
    pub fn enforce_limits(&mut self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut discarded = Vec::new();
        loop {
            let size = self.size();
            let exceeded = [
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
            .find_map(|(state, limit, count, bytes)| {
                limit.is_exceeded(count, bytes).then_some(state)
            })
            .or_else(|| {
                self.config.queued_limit.is_exceeded(size.queued, size.queued_size).then_some(
                    LaneTransactionState::Queued(LaneGap::Missing { expected: 0, found: 0 }),
                )
            });
            let Some(exceeded) = exceeded else { break };
            let victim = self
                .classified_transactions()
                .filter(|(state, transaction)| {
                    Self::same_subpool(*state, exceeded)
                        && !self
                            .config
                            .local_transactions_config
                            .is_local(transaction.origin, transaction.sender_ref())
                })
                .map(|(_, transaction)| transaction)
                .min_by_key(|transaction| {
                    (
                        transaction.transaction.max_fee_per_gas(),
                        transaction.transaction.priority_fee_or_price(),
                        *transaction.hash(),
                    )
                })
                .map(|transaction| *transaction.hash());
            let Some(victim) = victim else { break };
            discarded.extend(self.remove_with_descendants(&[victim]));
        }
        discarded
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
        replacing: bool,
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
        if self.senders.get(&sender).map_or(0, HashSet::len) >= self.config.max_account_slots {
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
        self.hashes.insert(hash, Arc::clone(&transaction));
        self.identities.insert(identity, transaction);
    }

    fn remove_indexed(
        &mut self,
        identity: BaseTransactionIdentity,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        let transaction = self.identities.remove(&identity)?;
        match identity {
            BaseTransactionIdentity::Nonce { lane, nonce } => {
                self.lanes.get_mut(&lane)?.transactions.remove(&nonce);
            }
            BaseTransactionIdentity::Replay { replay_id } => {
                self.replays.remove(&replay_id);
            }
        }
        self.hashes.remove(transaction.hash());
        let sender = transaction.sender();
        if let Some(hashes) = self.senders.get_mut(&sender) {
            hashes.remove(transaction.hash());
            if hashes.is_empty() {
                self.senders.remove(&sender);
            }
        }
        Some(transaction)
    }

    fn classified_transactions(
        &self,
    ) -> impl Iterator<Item = (LaneTransactionState, &Arc<ValidPoolTransaction<T>>)> {
        self.lanes.values().flat_map(|lane| lane.classified(self.base_fee)).chain(
            self.replays.values().map(|transaction| {
                let state = if transaction.transaction.max_fee_per_gas() < u128::from(self.base_fee)
                {
                    LaneTransactionState::BaseFee
                } else {
                    LaneTransactionState::Pending
                };
                (state, transaction)
            }),
        )
    }

    fn states_by_hash(&self) -> B256Map<LaneTransactionState> {
        self.classified_transactions()
            .map(|(state, transaction)| (*transaction.hash(), state))
            .collect()
    }

    fn pending_changes(
        &self,
        before: &B256Map<LaneTransactionState>,
    ) -> (Vec<StoredTransaction<T>>, Vec<DemotedTransaction<T>>) {
        let mut promoted = Vec::new();
        let mut demoted = Vec::new();
        for (state, transaction) in self.classified_transactions() {
            match (before.get(transaction.hash()), state) {
                (Some(previous), LaneTransactionState::Pending)
                    if *previous != LaneTransactionState::Pending =>
                {
                    promoted.push(Arc::clone(transaction));
                }
                (Some(LaneTransactionState::Pending), LaneTransactionState::BaseFee) => {
                    demoted.push((Arc::clone(transaction), SubPool::BaseFee));
                }
                (Some(LaneTransactionState::Pending), LaneTransactionState::Queued(_)) => {
                    demoted.push((Arc::clone(transaction), SubPool::Queued));
                }
                _ => {}
            }
        }
        (promoted, demoted)
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
                | (LaneTransactionState::Queued(_), LaneTransactionState::Queued(_))
        )
    }

    const fn added_state(state: LaneTransactionState) -> AddedTransactionState {
        match state {
            LaneTransactionState::Pending => AddedTransactionState::Pending,
            LaneTransactionState::BaseFee => {
                AddedTransactionState::Queued(QueuedReason::InsufficientBaseFee)
            }
            LaneTransactionState::Queued(_) => {
                AddedTransactionState::Queued(QueuedReason::NonceGap)
            }
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
                    .take_while(|(state, _)| *state == LaneTransactionState::Pending)
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
        Self { lanes, candidates, indexes, ordering, base_fee }
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
            lane.index += 1;
            self.push_head(index);
            return Some(transaction);
        }
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

    fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
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
    use reth_transaction_pool::{
        PoolTransaction, TransactionOrigin,
        identifier::{SenderId, TransactionId},
        pool::PendingPool,
        test_utils::TransactionBuilder,
    };

    use super::*;
    use crate::{BaseOrdering, BasePooledTransaction};

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
        LaneTransactionStore::new(
            PoolConfig { max_account_slots: usize::MAX, ..PoolConfig::default() },
            base_fee,
        )
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
}
