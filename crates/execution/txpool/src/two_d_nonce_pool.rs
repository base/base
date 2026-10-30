//! Lane storage and iteration for transactions kept outside the protocol
//! pool: the EIP-8130 sidecar (finite nonce channels and nonce-free
//! transactions) and the validity pool, which also holds protocol-nonce lanes.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap, HashSet},
    ops::Bound,
    sync::Arc,
    time::Instant,
};

use alloy_primitives::{
    Address, B256, TxHash, U256, U512,
    map::{AddressMap, B256Map, HashMap},
};
use base_common_consensus::Eip8130Constants;
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_transaction_pool::{
    AddedTransactionOutcome, BestTransactions, LocalTransactionConfig, PoolConfig, PoolResult,
    PoolSize, PriceBumpConfig, SubPoolLimit, TransactionOrdering, ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolError, PoolErrorKind},
    identifier::{SenderId, TransactionId},
    pool::{AddedTransactionState, QueuedReason},
};

use crate::{BasePooledTx, BestTransactionPriority, best::MergeSource};

/// A finite channel: `(sender, nonce_key)`.
pub(crate) type LaneId = (Address, U256);

/// Where a lane's cursor stands relative to canonical state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LaneCursor {
    /// The channel nonce as validation or canonical maintenance last observed it.
    pub state_nonce: u64,
    /// The first nonce this lane can execute. Equal to `state_nonce` unless
    /// another store holds the pending run from `state_nonce` up to here.
    pub next_nonce: u64,
}

impl LaneCursor {
    /// A cursor sitting at the canonical nonce.
    pub(crate) const fn at(state_nonce: u64) -> Self {
        Self { state_nonce, next_nonce: state_nonce }
    }
}

#[derive(Debug)]
struct NonceLane<T: BasePooledTx> {
    /// Canonical nonce the lane was last anchored to.
    state_nonce: u64,
    /// First executable nonce; see [`LaneCursor::next_nonce`].
    next_nonce: u64,
    /// Length of the consecutive pending run from `next_nonce`. Kept in step
    /// by every mutation, so pending and queued splits never walk the lane.
    pending_len: u64,
    transactions: BTreeMap<u64, Arc<ValidPoolTransaction<T>>>,
}

impl<T: BasePooledTx> NonceLane<T> {
    /// A lane at `cursor` holding the members of `transactions` at or past
    /// its executable cursor; members below it are dropped. No member sits
    /// below `next_nonce` afterwards either: insertion rejects such nonces and
    /// [`Self::set_next_nonce`] drops them.
    fn new(
        cursor: LaneCursor,
        mut transactions: BTreeMap<u64, Arc<ValidPoolTransaction<T>>>,
    ) -> Self {
        let transactions = transactions.split_off(&cursor.next_nonce);
        let mut lane = Self {
            state_nonce: cursor.state_nonce,
            next_nonce: cursor.next_nonce,
            pending_len: 0,
            transactions,
        };
        lane.extend_pending_run();
        lane
    }

    /// The nonce `offset` places past `next_nonce`, unless that overflows.
    const fn nonce_at(&self, offset: u64) -> Option<u64> {
        self.next_nonce.checked_add(offset)
    }

    /// Grows the pending run over every member directly following it.
    fn extend_pending_run(&mut self) {
        while self
            .nonce_at(self.pending_len)
            .is_some_and(|nonce| self.transactions.contains_key(&nonce))
        {
            self.pending_len += 1;
        }
    }

    fn is_pending(&self, nonce: u64) -> bool {
        nonce.checked_sub(self.next_nonce).is_some_and(|offset| offset < self.pending_len)
    }

    fn insert(
        &mut self,
        nonce: u64,
        transaction: Arc<ValidPoolTransaction<T>>,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        let replaced = self.transactions.insert(nonce, transaction);
        if self.nonce_at(self.pending_len) == Some(nonce) {
            self.extend_pending_run();
        }
        replaced
    }

    fn remove(&mut self, nonce: u64) -> Option<Arc<ValidPoolTransaction<T>>> {
        let removed = self.transactions.remove(&nonce)?;
        if self.is_pending(nonce) {
            self.pending_len = nonce - self.next_nonce;
        }
        Some(removed)
    }

    /// Moves the executable cursor, returning the members left below it.
    fn set_next_nonce(&mut self, next_nonce: u64) -> BTreeMap<u64, Arc<ValidPoolTransaction<T>>> {
        let retained = self.transactions.split_off(&next_nonce);
        let stale = std::mem::replace(&mut self.transactions, retained);
        self.next_nonce = next_nonce;
        self.pending_len = 0;
        self.extend_pending_run();
        stale
    }

    /// Key bound of the first member at or past `next_nonce + offset`; empty
    /// when that lies past `u64::MAX`.
    fn offset_start(&self, offset: u64) -> Bound<u64> {
        self.nonce_at(offset).map_or(Bound::Excluded(u64::MAX), Bound::Included)
    }

    /// Key bound right after the consecutive pending run; open when the run
    /// reaches `u64::MAX`.
    fn run_end(&self) -> Bound<u64> {
        self.nonce_at(self.pending_len).map_or(Bound::Unbounded, Bound::Excluded)
    }

    fn live_transactions(&self) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.transactions.range(self.next_nonce..).map(|(_, transaction)| transaction)
    }

    fn consecutive_pending_transactions(
        &self,
    ) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.transactions
            .range((Bound::Included(self.next_nonce), self.run_end()))
            .map(|(_, transaction)| transaction)
    }

    const fn consecutive_pending_len(&self) -> usize {
        self.pending_len as usize
    }

    fn queued_transactions(&self) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.transactions
            .range((self.offset_start(self.pending_len), Bound::Unbounded))
            .map(|(_, transaction)| transaction)
    }

    /// Members queued behind the pending run. Insertion rejects nonces below
    /// `next_nonce` and moving it drops them, so every member is live.
    fn queued_len(&self) -> usize {
        self.transactions.len() - self.consecutive_pending_len()
    }

    /// Whether every live member is in the pending run.
    fn is_fully_pending(&self) -> bool {
        self.transactions.last_key_value().is_none_or(|(nonce, _)| self.is_pending(*nonce))
    }

    /// The nonce right after the lane's consecutive pending run, or `None`
    /// when nothing is pending.
    fn pending_end(&self) -> Option<u64> {
        (self.pending_len > 0).then(|| self.next_nonce.saturating_add(self.pending_len))
    }

    const fn cursor(&self) -> LaneCursor {
        LaneCursor { state_nonce: self.state_nonce, next_nonce: self.next_nonce }
    }
}

/// Transactions whose lane state changed when a lane cursor moved.
#[derive(Debug)]
pub(crate) struct LaneUpdate<T: BasePooledTx> {
    /// Removed because their nonce is below the new cursor and can never execute.
    pub discarded: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Joined the lane's consecutive pending run.
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Left the lane's consecutive pending run because the cursor moved back
    /// behind a gap.
    pub queued: Vec<Arc<ValidPoolTransaction<T>>>,
}

impl<T: BasePooledTx> Default for LaneUpdate<T> {
    fn default() -> Self {
        Self { discarded: Vec::new(), promoted: Vec::new(), queued: Vec::new() }
    }
}

impl<T: BasePooledTx> LaneUpdate<T> {
    pub(crate) fn extend(&mut self, other: Self) {
        self.discarded.extend(other.discarded);
        self.promoted.extend(other.promoted);
        self.queued.extend(other.queued);
    }
}

/// Outcome returned after inserting into the 2D nonce sidecar.
#[derive(Debug)]
pub(crate) struct InsertOutcome<T: BasePooledTx> {
    pub outcome: AddedTransactionOutcome,
    pub replaced: Option<Arc<ValidPoolTransaction<T>>>,
    /// Lane changes from anchoring the cursor to the validation state. These
    /// stand even if the insertion is rolled back.
    pub anchor: LaneUpdate<T>,
    /// Queued successors the inserted transaction made pending.
    pub promoted: Vec<Arc<ValidPoolTransaction<T>>>,
}

/// Outcome returned after pruning mined transactions from the 2D nonce sidecar.
#[derive(Debug)]
pub(crate) struct PruneMinedOutcome<T: BasePooledTx> {
    pub removed: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Lane changes caused by advancing cursors past the mined nonces.
    pub lanes: LaneUpdate<T>,
}

/// Admission and capacity limits for the sidecar, mirroring the protocol pool.
#[derive(Debug, Clone)]
pub(crate) struct SidecarLimits {
    /// Count and byte caps across all sidecar transactions.
    pub capacity: SubPoolLimit,
    /// Transactions one non-local sender may hold beyond its lane heads.
    pub max_account_slots: usize,
    /// Minimum `max_fee_per_gas` accepted, in wei.
    pub minimal_protocol_basefee: u64,
    /// Senders and origins exempt from the per-sender slot cap.
    pub local_transactions: LocalTransactionConfig,
}

impl SidecarLimits {
    /// Applies the protocol pool's fee floor, slot cap and pending subpool cap.
    pub(crate) fn from_pool_config(config: &PoolConfig) -> Self {
        Self {
            capacity: config.pending_limit,
            max_account_slots: config.max_account_slots,
            minimal_protocol_basefee: config.minimal_protocol_basefee,
            local_transactions: config.local_transactions_config.clone(),
        }
    }

    /// Limits that never reject or evict.
    pub(crate) fn unlimited() -> Self {
        Self {
            capacity: SubPoolLimit::new(usize::MAX, usize::MAX),
            max_account_slots: usize::MAX,
            minimal_protocol_basefee: 0,
            local_transactions: LocalTransactionConfig::default(),
        }
    }
}

/// Running totals checked against [`SidecarLimits`], the sender ids held by
/// live transactions, and each sender's summed `cost()` against its balance for
/// coinbase tip backing.
#[derive(Debug, Default)]
struct SidecarUsage {
    size: usize,
    per_sender: HashMap<Address, usize>,
    sender_ids: SidecarSenderIds,
    /// Exact sum of each sender's `cost()`s. Fewer than 2^64 values below
    /// 2^256 cannot overflow 512 bits, so additions and removals never saturate.
    sender_costs: HashMap<Address, U512>,
    /// Each sender's balance, `None` when unknown, and the balance epoch it was
    /// recorded at. Kept only for senders with sidecar transactions; an unknown
    /// or missing balance backs nothing.
    sender_balances: HashMap<Address, (Option<U256>, u64)>,
    /// Advanced by every batch of balance updates, so a balance read during
    /// validation can be ordered against them.
    balance_epoch: u64,
    /// Each sender's live transactions, so a change in the sender's backing
    /// reaches them without scanning the pool.
    sender_hashes: HashMap<Address, HashSet<TxHash>>,
    /// The backing last applied to each sender's transactions.
    applied_backing: HashMap<Address, bool>,
}

impl SidecarUsage {
    fn add<T: BasePooledTx>(&mut self, transaction: &ValidPoolTransaction<T>) {
        self.size += transaction.transaction.size();
        let sender = transaction.sender();
        *self.per_sender.entry(sender).or_default() += 1;
        *self.sender_costs.entry(sender).or_default() +=
            U512::from(*transaction.transaction.cost());
        self.sender_hashes.entry(sender).or_default().insert(*transaction.hash());
    }

    fn remove<T: BasePooledTx>(&mut self, transaction: &ValidPoolTransaction<T>) {
        self.size = self.size.saturating_sub(transaction.transaction.size());
        let sender = transaction.sender();
        if let Some(cost) = self.sender_costs.get_mut(&sender) {
            *cost -= U512::from(*transaction.transaction.cost());
        }
        if let Some(hashes) = self.sender_hashes.get_mut(&sender) {
            hashes.remove(transaction.hash());
        }
        if let Some(count) = self.per_sender.get_mut(&sender) {
            *count -= 1;
            if *count == 0 {
                self.per_sender.remove(&sender);
                self.sender_costs.remove(&sender);
                self.sender_balances.remove(&sender);
                self.sender_hashes.remove(&sender);
                self.applied_backing.remove(&sender);
            }
        }
        self.sender_ids.release(transaction.sender_id());
        for id in transaction.authority_ids.iter().flatten() {
            self.sender_ids.release(*id);
        }
    }

    /// Returns the senders whose balance was recorded.
    fn apply_balance_updates(
        &mut self,
        updates: impl IntoIterator<Item = (Address, U256)>,
    ) -> Vec<Address> {
        self.balance_epoch += 1;
        let mut recorded = Vec::new();
        for (sender, balance) in updates {
            if self.per_sender.contains_key(&sender) {
                self.sender_balances.insert(sender, (Some(balance), self.balance_epoch));
                recorded.push(sender);
            }
        }
        recorded
    }

    fn record_validated_balance(&mut self, sender: Address, balance: U256, epoch: u64) {
        if !self.per_sender.contains_key(&sender) {
            return;
        }
        let recorded = match self.sender_balances.get(&sender) {
            Some((_, recorded_epoch)) if *recorded_epoch > epoch => return,
            Some(_) => (Some(balance), epoch),
            None if epoch == self.balance_epoch => (Some(balance), epoch),
            // A newer update may have been dropped while the sender had no entry.
            None => (None, self.balance_epoch),
        };
        self.sender_balances.insert(sender, recorded);
    }

    /// Whether `sender`'s balance covers the summed `cost()` of all its sidecar
    /// transactions, which then all rank by their coinbase tips.
    fn tip_backed(&self, sender: &Address) -> bool {
        let cost = self.sender_costs.get(sender).copied().unwrap_or_default();
        self.sender_balances
            .get(sender)
            .and_then(|(balance, _)| *balance)
            .is_some_and(|balance| cost <= U512::from(balance))
    }

    fn sender_count(&self, sender: &Address) -> usize {
        self.per_sender.get(sender).copied().unwrap_or_default()
    }
}

/// Sender ids for sidecar transactions, counted by the live transactions that
/// reference each address as their sender or payer.
///
/// Reth's `SenderIdentifiers` never releases an id, so it would keep an entry
/// for every address the sidecar has ever seen. Here an id is released with
/// its last reference, and a returning address gets a fresh id.
#[derive(Debug, Default)]
pub struct SidecarSenderIds {
    next_id: u64,
    ids: AddressMap<(SenderId, usize)>,
    addresses: HashMap<SenderId, Address>,
}

impl SidecarSenderIds {
    /// Returns the id for `address` and records one more reference to it.
    pub fn acquire(&mut self, address: Address) -> SenderId {
        let (id, references) = self.ids.entry(address).or_insert_with(|| {
            let id = SenderId::from(self.next_id);
            self.next_id = self.next_id.wrapping_add(1);
            self.addresses.insert(id, address);
            (id, 0)
        });
        *references += 1;
        *id
    }

    /// Drops one reference to `id`, releasing the id with its last reference.
    pub fn release(&mut self, id: SenderId) {
        let Some(address) = self.addresses.get(&id).copied() else {
            return;
        };
        let Some((_, references)) = self.ids.get_mut(&address) else {
            return;
        };
        *references -= 1;
        if *references == 0 {
            self.ids.remove(&address);
            self.addresses.remove(&id);
        }
    }

    /// Returns the number of addresses that currently hold an id.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Returns `true` when no address holds an id.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

/// Eviction rank of a candidate (a lane tail or a nonce-free transaction); the
/// smallest is evicted first. Field order is the ranking order: queued before
/// pending, non-local before local, lowest fee cap, then most recently received.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct EvictionKey {
    pending: bool,
    local: bool,
    max_fee_per_gas: u128,
    received: Reverse<Instant>,
    hash: TxHash,
}

/// EIP-8130 sidecar for finite non-zero nonce channels and nonce-free transactions.
///
/// Finite channels are kept in ordered `(sender, nonce_key)` lanes. Nonce-free
/// transactions have no sequencing relationship, so they are stored separately
/// by replay id and compete independently in the best-transactions iterator.
#[derive(Debug)]
pub(crate) struct TwoDNoncePool<T: BasePooledTx> {
    lanes: HashMap<LaneId, NonceLane<T>>,
    nonce_free: B256Map<Arc<ValidPoolTransaction<T>>>,
    hashes: B256Map<Arc<ValidPoolTransaction<T>>>,
    price_bump_config: PriceBumpConfig,
    limits: SidecarLimits,
    usage: SidecarUsage,
    /// Whether transactions sequenced on the account's protocol nonce (nonce
    /// key zero, including non-EIP-8130 types) are stored in `(sender, 0)`
    /// lanes. The EIP-8130 sidecar rejects them; the validity pool accepts them.
    protocol_nonce_lanes: bool,
    /// Every eviction candidate, ordered by [`EvictionKey`]. Kept in step with
    /// `lanes` and `nonce_free` so eviction never scans the pool.
    eviction_order: BTreeSet<EvictionKey>,
    /// The key each lane's tail currently holds in `eviction_order`.
    lane_candidates: HashMap<LaneId, EvictionKey>,
    /// Lanes whose executable cursor sits past their canonical anchor, with
    /// that anchor. Kept in step with `lanes` so a build never scans them.
    ahead_of_state: HashMap<LaneId, u64>,
    /// Lanes another store also holds transactions in, flagged by the owner
    /// when the two meet. Cleared with the lane; a flag may outlive the other
    /// store's transactions, so readers verify before relying on it.
    shared_lanes: HashSet<LaneId>,
}

impl<T: BasePooledTx> TwoDNoncePool<T> {
    /// Creates a new 2D nonce sidecar pool without admission limits.
    pub(crate) fn new(price_bump_config: PriceBumpConfig) -> Self {
        Self {
            lanes: HashMap::default(),
            nonce_free: B256Map::default(),
            hashes: B256Map::default(),
            price_bump_config,
            limits: SidecarLimits::unlimited(),
            usage: SidecarUsage::default(),
            protocol_nonce_lanes: false,
            eviction_order: BTreeSet::new(),
            lane_candidates: HashMap::default(),
            ahead_of_state: HashMap::default(),
            shared_lanes: HashSet::default(),
        }
    }

    /// Applies admission and capacity limits. Call before inserting.
    #[must_use]
    pub(crate) fn with_limits(mut self, limits: SidecarLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Also stores transactions sequenced on the account's protocol nonce, in
    /// `(sender, 0)` lanes. Call before inserting.
    #[must_use]
    pub(crate) const fn with_protocol_nonce_lanes(mut self) -> Self {
        self.protocol_nonce_lanes = true;
        self
    }

    /// The nonce key of the lane `transaction` is sequenced in: its EIP-8130
    /// channel, or zero for a protocol-nonce transaction when this pool stores
    /// those. `None` for nonce-free transactions and for protocol-nonce
    /// transactions the pool does not accept.
    fn lane_key(&self, transaction: &T) -> Option<U256> {
        if transaction.is_eip8130_nonce_free() {
            return None;
        }
        transaction
            .eip8130_nonce_channel_key()
            .or_else(|| self.protocol_nonce_lanes.then_some(U256::ZERO))
    }

    /// Returns the summed `size()` of every pooled transaction.
    pub(crate) const fn size(&self) -> usize {
        self.usage.size
    }

    /// Returns the number of pooled transactions.
    pub(crate) fn len(&self) -> usize {
        self.hashes.len()
    }

    /// Rejects a new transaction the protocol pool would refuse: a fee cap below
    /// the protocol floor, or a transaction from a non-local sender that already
    /// fills its slots. Replacements never add a slot.
    ///
    /// Reth exempts an account's next nonce from the slot cap. A sender can open
    /// any number of lanes, each with its own head, so only the head of a lane
    /// already holding queued transactions is exempt; a head on a fresh lane
    /// counts against the cap.
    fn ensure_admissible(
        &self,
        transaction: &ValidPoolTransaction<T>,
        is_replacement: bool,
        fills_existing_lane_head: bool,
    ) -> PoolResult<()> {
        let hash = *transaction.hash();
        let fee_cap = transaction.max_fee_per_gas();
        if fee_cap < u128::from(self.limits.minimal_protocol_basefee) {
            return Err(PoolError::new(
                hash,
                PoolErrorKind::FeeCapBelowMinimumProtocolFeeCap(fee_cap),
            ));
        }
        let sender = transaction.sender();
        if !is_replacement
            && !fills_existing_lane_head
            && !self.limits.local_transactions.is_local(transaction.origin, &sender)
            && self.usage.sender_count(&sender) >= self.limits.max_account_slots
        {
            return Err(PoolError::new(hash, PoolErrorKind::SpammerExceededCapacity(sender)));
        }
        Ok(())
    }

    /// Evicts the lowest-priority transactions until the sidecar fits its
    /// capacity, returning them so callers can release their guard slots.
    ///
    /// Only lane tails and nonce-free transactions are candidates, so eviction
    /// never opens a nonce gap. Queued tails (behind a nonce gap) go first: they
    /// cannot execute and cost nothing to hold, so ranking them by fee alone
    /// would let gapped spam push out pending transactions. See [`EvictionKey`]
    /// for the full order. Each eviction is `O(log n)`.
    pub(crate) fn evict_over_capacity(&mut self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut evicted = Vec::new();
        while self.limits.capacity.is_exceeded(self.hashes.len(), self.usage.size)
            && let Some(hash) = self.eviction_candidate()
            && let Some(transaction) = self.remove_hash(hash)
        {
            evicted.push(transaction);
        }
        evicted
    }

    fn eviction_candidate(&self) -> Option<TxHash> {
        self.eviction_order.first().map(|key| key.hash)
    }

    fn eviction_key(&self, transaction: &ValidPoolTransaction<T>, pending: bool) -> EvictionKey {
        EvictionKey {
            pending,
            local: self
                .limits
                .local_transactions
                .is_local(transaction.origin, &transaction.sender()),
            max_fee_per_gas: transaction.max_fee_per_gas(),
            received: Reverse(transaction.timestamp),
            hash: *transaction.hash(),
        }
    }

    /// Re-ranks `lane_id`'s tail after the lane changed. Must follow every lane
    /// mutation: the tail, and whether it is pending, depend on the whole lane.
    /// Brings every per-lane index in line with `lane_id` after it changed:
    /// its eviction candidate, its ahead-of-state anchor and its shared flag.
    fn refresh_lane_indexes(&mut self, lane_id: LaneId) {
        if let Some(previous) = self.lane_candidates.remove(&lane_id) {
            self.eviction_order.remove(&previous);
        }
        let Some(lane) = self.lanes.get(&lane_id) else {
            self.ahead_of_state.remove(&lane_id);
            self.shared_lanes.remove(&lane_id);
            return;
        };
        if lane.next_nonce > lane.state_nonce {
            self.ahead_of_state.insert(lane_id, lane.state_nonce);
        } else {
            self.ahead_of_state.remove(&lane_id);
        }
        if let Some((_, tail)) = lane.transactions.last_key_value() {
            let key = self.eviction_key(tail, lane.is_fully_pending());
            self.eviction_order.insert(key);
            self.lane_candidates.insert(lane_id, key);
        }
    }

    /// Returns true if the sidecar already contains the hash.
    pub(crate) fn contains(&self, hash: &TxHash) -> bool {
        self.hashes.contains_key(hash)
    }

    /// Returns the number of pending and queued transactions.
    pub(crate) fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        let (pending, queued) = self.lanes.values().fold((0, 0), |(pending, queued), lane| {
            (pending + lane.consecutive_pending_len(), queued + lane.queued_len())
        });
        (pending + self.nonce_free.len(), queued)
    }

    /// Counts and byte sizes of the pending and queued sets, in one pass over
    /// the transactions and without cloning anything.
    pub(crate) fn pool_size(&self) -> PoolSize {
        let mut size = PoolSize::default();
        for lane in self.lanes.values() {
            for transaction in lane.transactions.values() {
                if lane.is_pending(transaction.nonce()) {
                    size.pending += 1;
                    size.pending_size += transaction.transaction.size();
                } else {
                    size.queued += 1;
                    size.queued_size += transaction.transaction.size();
                }
            }
        }
        for transaction in self.nonce_free.values() {
            size.pending += 1;
            size.pending_size += transaction.transaction.size();
        }
        size.total = size.pending + size.queued;
        size
    }

    /// Every pending transaction, lane runs first and nonce-free ones last.
    pub(crate) fn pending_transactions_iter(
        &self,
    ) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.lanes
            .values()
            .flat_map(NonceLane::consecutive_pending_transactions)
            .chain(self.nonce_free.values())
    }

    /// Every queued transaction.
    pub(crate) fn queued_transactions_iter(
        &self,
    ) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.lanes.values().flat_map(NonceLane::queued_transactions)
    }

    /// Every transaction, lane members first and nonce-free ones last.
    pub(crate) fn all_transactions_iter(
        &self,
    ) -> impl Iterator<Item = &Arc<ValidPoolTransaction<T>>> {
        self.lanes.values().flat_map(NonceLane::live_transactions).chain(self.nonce_free.values())
    }

    /// Returns all pending transactions.
    pub(crate) fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.pending_transactions_iter().cloned().collect()
    }

    /// Returns all queued transactions.
    pub(crate) fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.queued_transactions_iter().cloned().collect()
    }

    /// Returns all transactions in the sidecar.
    pub(crate) fn all_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.all_transactions_iter().cloned().collect()
    }

    /// Returns all transaction hashes in the sidecar.
    pub(crate) fn all_hashes(&self) -> Vec<TxHash> {
        let mut hashes = Vec::new();
        for lane in self.lanes.values() {
            hashes.extend(lane.live_transactions().map(|transaction| *transaction.hash()));
        }
        hashes.extend(self.nonce_free.values().map(|transaction| *transaction.hash()));
        hashes
    }

    /// Returns the transaction for the given hash.
    pub(crate) fn get(&self, hash: &TxHash) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.hashes.get(hash).cloned()
    }

    /// Returns transactions for the given sender.
    pub(crate) fn transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions = Vec::new();
        for ((lane_sender, _), lane) in &self.lanes {
            if *lane_sender == sender {
                transactions.extend(lane.live_transactions().cloned());
            }
        }
        transactions.extend(
            self.nonce_free.values().filter(|transaction| transaction.sender() == sender).cloned(),
        );
        transactions
    }

    /// Returns pending transactions for the given sender.
    pub(crate) fn pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut transactions: Vec<_> = self
            .lanes
            .iter()
            .filter(|((lane_sender, _), _)| *lane_sender == sender)
            .flat_map(|(_, lane)| lane.consecutive_pending_transactions())
            .cloned()
            .collect();
        transactions.extend(
            self.nonce_free.values().filter(|transaction| transaction.sender() == sender).cloned(),
        );
        transactions
    }

    /// Returns queued transactions for the given sender.
    pub(crate) fn queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        self.lanes
            .iter()
            .filter(|((lane_sender, _), _)| *lane_sender == sender)
            .flat_map(|(_, lane)| lane.queued_transactions())
            .cloned()
            .collect()
    }

    /// Returns all senders present in the sidecar.
    pub(crate) fn unique_senders(&self) -> HashSet<Address> {
        self.lanes
            .keys()
            .map(|(sender, _)| *sender)
            .chain(self.nonce_free.values().map(|transaction| transaction.sender()))
            .collect()
    }

    /// Assigns the transaction's sender id and, for a sponsored transaction,
    /// its payer's authority id, each holding a reference until the transaction
    /// leaves the sidecar.
    ///
    /// The payer is the one admission resolved. Ids already on the transaction
    /// are replaced, so a re-inserted transaction never carries ids that were
    /// released.
    fn acquire_ids(&mut self, transaction: &mut ValidPoolTransaction<T>) {
        let sender = transaction.sender();
        let sender_id = self.usage.sender_ids.acquire(sender);
        transaction.transaction_id = TransactionId::new(sender_id, transaction.nonce());
        transaction.authority_ids = transaction
            .transaction
            .limit_class()
            .filter(|class| class.payer != sender)
            .map(|class| vec![self.usage.sender_ids.acquire(class.payer)]);
    }

    /// Inserts a validated transaction, anchoring its lane at `state_nonce`.
    ///
    /// Nonce-free transactions replace only another transaction with the same
    /// replay id. Lane transactions retain their lane-local sequence
    /// replacement semantics. The pool assigns the transaction's sender and
    /// authority ids.
    #[cfg(test)]
    pub(crate) fn insert_validated(
        &mut self,
        transaction: ValidPoolTransaction<T>,
        state_nonce: u64,
    ) -> PoolResult<InsertOutcome<T>> {
        self.insert_validated_at(transaction, LaneCursor::at(state_nonce))
    }

    /// [`Self::insert_validated`] with the lane anchored at `cursor`, whose
    /// `next_nonce` may sit past `state_nonce` when another store holds the
    /// pending run in between. A lane transaction below `cursor.next_nonce`
    /// is rejected as already executed.
    pub(crate) fn insert_validated_at(
        &mut self,
        mut transaction: ValidPoolTransaction<T>,
        cursor: LaneCursor,
    ) -> PoolResult<InsertOutcome<T>> {
        let hash = *transaction.hash();
        if self.contains(&hash) {
            return Err(PoolError::new(hash, PoolErrorKind::AlreadyImported));
        }

        if let Some(replay_id) = transaction.transaction.eip8130_replay_id() {
            self.ensure_admissible(&transaction, self.nonce_free.contains_key(&replay_id), false)?;
            let replaced = if let Some(existing) = self.nonce_free.get(&replay_id) {
                if existing.is_replacement_underpriced(&transaction, &self.price_bump_config) {
                    return Err(PoolError::new(hash, PoolErrorKind::ReplacementUnderpriced));
                }
                Some(Arc::clone(existing))
            } else {
                None
            };
            self.acquire_ids(&mut transaction);
            let transaction = Arc::new(transaction);
            if let Some(existing) = &replaced {
                self.hashes.remove(existing.hash());
                self.usage.remove(existing);
                self.eviction_order.remove(&self.eviction_key(existing, true));
            }
            self.nonce_free.insert(replay_id, Arc::clone(&transaction));
            self.usage.add(&transaction);
            self.eviction_order.insert(self.eviction_key(&transaction, true));
            self.hashes.insert(hash, Arc::clone(&transaction));
            let sender = transaction.sender();
            self.refresh_tip_backing(sender);
            transaction.transaction.set_coinbase_tip_backed(self.usage.tip_backed(&sender));
            return Ok(InsertOutcome {
                outcome: AddedTransactionOutcome { hash, state: AddedTransactionState::Pending },
                replaced,
                anchor: LaneUpdate::default(),
                promoted: Vec::new(),
            });
        }

        let sender = transaction.sender();
        let nonce_key = self.lane_key(&transaction.transaction).ok_or_else(|| {
            PoolError::other(hash, "2D nonce pool only accepts channelized EIP-8130 transactions")
        })?;

        let lane_id = (sender, nonce_key);
        let nonce = transaction.nonce();
        let existing_lane = self.lanes.get(&lane_id).filter(|lane| !lane.transactions.is_empty());
        let is_replacement =
            existing_lane.is_some_and(|lane| lane.transactions.contains_key(&nonce));
        let fills_existing_lane_head = existing_lane.is_some() && nonce == cursor.next_nonce;
        self.ensure_admissible(&transaction, is_replacement, fills_existing_lane_head)?;
        if nonce < cursor.next_nonce {
            return Err(PoolError::new(
                hash,
                PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::Consensus(
                    InvalidTransactionError::NonceNotConsistent {
                        tx: nonce,
                        state: cursor.next_nonce,
                    },
                )),
            ));
        }
        let replaced = self
            .lanes
            .get(&lane_id)
            .and_then(|lane| lane.transactions.get(&nonce))
            .map(|existing| {
                if existing.is_replacement_underpriced(&transaction, &self.price_bump_config) {
                    Err(PoolError::new(hash, PoolErrorKind::ReplacementUnderpriced))
                } else {
                    Ok(Arc::clone(existing))
                }
            })
            .transpose()?;
        self.acquire_ids(&mut transaction);
        let transaction = Arc::new(transaction);

        // Keep the lane anchored to the state view used by validation. This may
        // move backward after a reorg lowers the on-chain channel nonce, allowing
        // now-valid transactions to be accepted instead of treating them as
        // already executed under the pre-reorg lane cursor.
        let anchor = self.set_lane_cursor(lane_id, cursor);
        let lane =
            self.lanes.entry(lane_id).or_insert_with(|| NonceLane::new(cursor, BTreeMap::new()));
        let pending_len_before = lane.pending_len;

        lane.insert(nonce, Arc::clone(&transaction));
        self.hashes.insert(hash, Arc::clone(&transaction));
        self.usage.add(&transaction);

        if let Some(replaced) = &replaced {
            let replaced_hash = *replaced.hash();
            self.hashes.remove(&replaced_hash);
            self.usage.remove(replaced);
        }

        let state = if lane.is_pending(nonce) {
            AddedTransactionState::Pending
        } else {
            AddedTransactionState::Queued(QueuedReason::NonceGap)
        };

        // Members the newcomer closed a gap in front of join the pending run with it.
        let promoted = lane
            .transactions
            .range((lane.offset_start(pending_len_before), lane.run_end()))
            .map(|(_, candidate)| candidate)
            .filter(|candidate| *candidate.hash() != hash)
            .cloned()
            .collect();

        self.refresh_lane_indexes(lane_id);
        self.refresh_tip_backing(sender);
        transaction.transaction.set_coinbase_tip_backed(self.usage.tip_backed(&sender));
        Ok(InsertOutcome {
            outcome: AddedTransactionOutcome { hash, state },
            replaced,
            anchor,
            promoted,
        })
    }

    /// Moves a lane's canonical anchor forward to `nonce`, leaving it alone
    /// when the anchor is already there or past it. The executable cursor
    /// never moves back below where it stood.
    ///
    /// Canonical maintenance runs independently of validation, so a late update
    /// for an older block can report a nonce an insert has already anchored
    /// past. Only validation anchoring and authoritative account nonces (a
    /// reorg or reth's account resync) move a cursor backward, through
    /// [`Self::set_lane_cursor`].
    pub(crate) fn advance_lane_nonce(&mut self, lane_id: LaneId, nonce: u64) -> LaneUpdate<T> {
        match self.lanes.get(&lane_id) {
            Some(lane) if lane.state_nonce < nonce => self.set_lane_cursor(
                lane_id,
                LaneCursor { state_nonce: nonce, next_nonce: lane.next_nonce.max(nonce) },
            ),
            _ => LaneUpdate::default(),
        }
    }

    /// Moves a lane's cursor to its canonical `nonce`, as validation or a
    /// canonical state change observed it.
    #[cfg(test)]
    pub(crate) fn set_lane_nonce(&mut self, lane_id: LaneId, nonce: u64) -> LaneUpdate<T> {
        self.set_lane_cursor(lane_id, LaneCursor::at(nonce))
    }

    /// Moves a lane to `cursor`, in either direction.
    ///
    /// Transactions below the new executable cursor can never execute here, so
    /// they are removed. The returned update also lists the transactions that
    /// joined the consecutive pending run and those that left it because the
    /// cursor moved back behind a gap, so listeners can follow the lane's state.
    pub(crate) fn set_lane_cursor(&mut self, lane_id: LaneId, cursor: LaneCursor) -> LaneUpdate<T> {
        let mut update = LaneUpdate::default();
        let Some(lane) = self.lanes.get_mut(&lane_id) else {
            return update;
        };
        if lane.cursor() == cursor {
            return update;
        }
        lane.state_nonce = cursor.state_nonce;
        if lane.next_nonce == cursor.next_nonce {
            self.refresh_lane_indexes(lane_id);
            return update;
        }
        // A pending run is a contiguous nonce range, so only members between
        // the two runs' starts or between their ends can have changed state;
        // the shared middle stays pending and is never visited. Where the two
        // spans overlap, the runs are disjoint and the overlap lies in the gap
        // between them, so nothing there is reported.
        let (old_next_nonce, old_pending_len) = (lane.next_nonce, lane.pending_len);
        let old_end = lane.run_end();
        let was_pending = |nonce: u64| {
            nonce.checked_sub(old_next_nonce).is_some_and(|offset| offset < old_pending_len)
        };
        let stale = lane.set_next_nonce(cursor.next_nonce);
        for transaction in stale.into_values() {
            self.hashes.remove(transaction.hash());
            self.usage.remove(&transaction);
            update.discarded.push(transaction);
        }
        let new_end = lane.run_end();
        let starts = old_next_nonce.min(lane.next_nonce)..old_next_nonce.max(lane.next_nonce);
        let ends = match (old_end, new_end) {
            (Bound::Excluded(a), Bound::Excluded(b)) => {
                Some((Bound::Included(a.min(b)), Bound::Excluded(a.max(b))))
            }
            (Bound::Excluded(end), Bound::Unbounded) | (Bound::Unbounded, Bound::Excluded(end)) => {
                Some((Bound::Included(end), Bound::Unbounded))
            }
            _ => None,
        };
        let changed = lane
            .transactions
            .range(starts)
            .chain(ends.into_iter().flat_map(|ends| lane.transactions.range(ends)));
        for (nonce, transaction) in changed {
            match (was_pending(*nonce), lane.is_pending(*nonce)) {
                (false, true) => update.promoted.push(Arc::clone(transaction)),
                (true, false) => update.queued.push(Arc::clone(transaction)),
                _ => {}
            }
        }
        if lane.transactions.is_empty() {
            self.lanes.remove(&lane_id);
        }
        self.refresh_lane_indexes(lane_id);
        if !update.discarded.is_empty() {
            self.refresh_tip_backing(lane_id.0);
        }
        update
    }

    /// Returns where `lane_id`'s cursor stands, or `None` when the lane is empty.
    pub(crate) fn lane_cursor(&self, lane_id: &LaneId) -> Option<LaneCursor> {
        self.lanes.get(lane_id).map(NonceLane::cursor)
    }

    /// Returns the nonce right after `lane_id`'s consecutive pending run, or
    /// `None` when the lane has nothing pending.
    pub(crate) fn lane_pending_end(&self, lane_id: &LaneId) -> Option<u64> {
        self.lanes.get(lane_id).and_then(NonceLane::pending_end)
    }

    /// Returns the live transaction at `nonce` in `lane_id`.
    pub(crate) fn lane_transaction(
        &self,
        lane_id: &LaneId,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        let lane = self.lanes.get(lane_id)?;
        (nonce >= lane.next_nonce).then(|| lane.transactions.get(&nonce).cloned())?
    }

    /// Returns the transaction at `nonce` in `lane_id` if it sits in the lane's
    /// consecutive pending run.
    pub(crate) fn lane_pending_transaction(
        &self,
        lane_id: &LaneId,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        let lane = self.lanes.get(lane_id)?;
        let pending_end = lane.pending_end()?;
        (lane.next_nonce..pending_end)
            .contains(&nonce)
            .then(|| lane.transactions.get(&nonce).cloned())?
    }

    /// Returns the live transaction with the highest nonce in `lane_id`.
    pub(crate) fn lane_highest_transaction(
        &self,
        lane_id: &LaneId,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.lanes.get(lane_id)?.live_transactions().last().cloned()
    }

    /// Returns the last transaction of the consecutive run that starts at
    /// `from` in `lane_id`, if `from` itself is live there.
    pub(crate) fn highest_consecutive_from(
        &self,
        lane_id: &LaneId,
        from: u64,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        let lane = self.lanes.get(lane_id)?;
        if from < lane.next_nonce {
            return None;
        }
        let mut expected = from;
        let mut last = None;
        for (nonce, transaction) in lane.transactions.range(from..) {
            if *nonce != expected {
                break;
            }
            last = Some(Arc::clone(transaction));
            expected = expected.checked_add(1)?;
        }
        last
    }

    /// Returns the nonce-free transaction stored under `replay_id`.
    pub(crate) fn nonce_free_transaction(
        &self,
        replay_id: &B256,
    ) -> Option<Arc<ValidPoolTransaction<T>>> {
        self.nonce_free.get(replay_id).cloned()
    }

    /// Lanes whose executable cursor sits past their canonical anchor, i.e.
    /// those relying on another store's pending run, with that anchor.
    pub(crate) const fn lanes_ahead_of_state(&self) -> &HashMap<LaneId, u64> {
        &self.ahead_of_state
    }

    /// Flags `lane_id` as one another store also holds transactions in. No-op
    /// for a lane this pool does not hold.
    pub(crate) fn mark_shared_lane(&mut self, lane_id: LaneId) {
        if self.lanes.contains_key(&lane_id) {
            self.shared_lanes.insert(lane_id);
        }
    }

    /// Clears the shared flag once the other store holds nothing in `lane_id`.
    pub(crate) fn unmark_shared_lane(&mut self, lane_id: &LaneId) {
        self.shared_lanes.remove(lane_id);
    }

    /// Whether `lane_id` is flagged as shared with another store.
    pub(crate) fn is_shared_lane(&self, lane_id: &LaneId) -> bool {
        self.shared_lanes.contains(lane_id)
    }

    /// The shared lanes that currently have a pending run, with the nonce
    /// right after that run.
    pub(crate) fn shared_lanes_pending_end(&self) -> impl Iterator<Item = (LaneId, u64)> + '_ {
        self.shared_lanes.iter().filter_map(|lane_id| {
            self.lanes.get(lane_id).and_then(NonceLane::pending_end).map(|end| (*lane_id, end))
        })
    }

    /// Removes the exact transactions by hash without advancing lane state.
    pub(crate) fn remove_transactions(
        &mut self,
        hashes: &[TxHash],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut removed = Vec::new();
        for hash in hashes {
            if let Some(transaction) = self.remove_hash(*hash) {
                removed.push(transaction);
            }
        }
        removed
    }

    /// Removes transactions and their descendants for each hash.
    pub(crate) fn remove_transactions_and_descendants(
        &mut self,
        hashes: &[TxHash],
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut removed = Vec::new();
        for hash in hashes {
            if self
                .hashes
                .get(hash)
                .is_some_and(|transaction| transaction.transaction.is_eip8130_nonce_free())
            {
                if let Some(transaction) = self.remove_hash(*hash) {
                    removed.push(transaction);
                }
                continue;
            }
            let Some(transaction) = self.hashes.get(hash) else {
                continue;
            };
            let Some(nonce_key) = self.lane_key(&transaction.transaction) else {
                continue;
            };
            let lane_id = (transaction.sender(), nonce_key);
            let nonce = transaction.nonce();
            let Some(lane) = self.lanes.get(&lane_id) else {
                continue;
            };

            let descendant_hashes: Vec<_> = lane
                .transactions
                .range(nonce..)
                .map(|(_, transaction)| *transaction.hash())
                .collect();
            removed.extend(self.remove_transactions(&descendant_hashes));
        }
        removed
    }

    /// Prunes mined transactions and advances the matching lane heads.
    pub(crate) fn prune_mined(&mut self, hashes: &[TxHash]) -> PruneMinedOutcome<T> {
        let mut removed = Vec::new();
        for hash in hashes {
            if self
                .hashes
                .get(hash)
                .is_some_and(|transaction| transaction.transaction.is_eip8130_nonce_free())
                && let Some(transaction) = self.remove_hash(*hash)
            {
                removed.push(transaction);
            }
        }
        let mut ordered_hashes: Vec<_> = hashes
            .iter()
            .filter_map(|hash| {
                let transaction = self.hashes.get(hash)?;
                let nonce_key = self.lane_key(&transaction.transaction)?;
                Some((transaction.sender(), nonce_key, transaction.nonce(), *hash))
            })
            .collect();
        ordered_hashes.sort_unstable();

        // Mined nonces are consecutive per channel even where this pool holds
        // none of the intermediate hashes, so each lane resumes after its
        // highest mined nonce.
        let mut mined_heads: HashMap<LaneId, u64> = HashMap::default();
        for (sender, nonce_key, nonce, _) in &ordered_hashes {
            mined_heads.insert((*sender, *nonce_key), *nonce);
        }
        let mined: HashSet<TxHash> = ordered_hashes.iter().map(|(.., hash)| *hash).collect();
        let mut lanes = LaneUpdate::default();
        for (lane_id, nonce) in mined_heads {
            if let Some(next_nonce) = nonce.checked_add(1) {
                let mut update = self.advance_lane_nonce(lane_id, next_nonce);
                let (lane_mined, discarded) = update
                    .discarded
                    .into_iter()
                    .partition(|transaction| mined.contains(transaction.hash()));
                update.discarded = discarded;
                removed.extend::<Vec<_>>(lane_mined);
                lanes.extend(update);
            }
        }
        // Mined hashes at or past an already-advanced cursor.
        for (.., hash) in ordered_hashes {
            if let Some(transaction) = self.remove_hash(hash) {
                removed.push(transaction);
            }
        }

        PruneMinedOutcome { removed, lanes }
    }

    /// Removes nonce-free transactions whose validity window has elapsed at
    /// `now` (Unix **milliseconds**, i.e. `block.timestamp * 1000`).
    ///
    /// A nonce-free transaction is invalid when `valid_before <= now`, matching
    /// its structural validation rule. The bound is normalized to milliseconds
    /// (seconds are scaled by 1000) so it lines up with the millisecond `now`.
    /// Finite channels are unaffected; their optional window is handled by normal
    /// transaction validation until the state-keyed expiry index is introduced.
    pub(crate) fn remove_expired_nonce_free(
        &mut self,
        now: u64,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let expired: Vec<TxHash> = self
            .nonce_free
            .values()
            .filter_map(|transaction| {
                let signed = transaction.transaction.as_eip8130()?;
                (signed.tx().valid_before_ms() <= now).then_some(*transaction.hash())
            })
            .collect();
        self.remove_transactions(&expired)
    }

    /// Removes all transactions for the given sender.
    pub(crate) fn remove_transactions_by_sender(
        &mut self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let hashes: Vec<_> =
            self.hashes.values().filter(|tx| tx.sender() == sender).map(|tx| *tx.hash()).collect();
        self.remove_transactions(&hashes)
    }

    /// Returns the current balance epoch. Read it before validating a
    /// transaction and pass it to [`Self::record_validated_balance`].
    pub(crate) const fn balance_epoch(&self) -> u64 {
        self.usage.balance_epoch
    }

    /// Records a batch of current account balances, which back the coinbase
    /// tips of the senders' sidecar transactions, and advances the balance
    /// epoch. Senders with no sidecar transactions are ignored.
    pub(crate) fn apply_balance_updates(
        &mut self,
        updates: impl IntoIterator<Item = (Address, U256)>,
    ) {
        for sender in self.usage.apply_balance_updates(updates) {
            self.refresh_tip_backing(sender);
        }
    }

    /// Records the sender balance a validation read after observing balance
    /// epoch `epoch`.
    ///
    /// A balance recorded by a later update wins. A sender with no recorded
    /// balance takes the validation's balance only when no update has run since
    /// `epoch`, because an update for a sender without sidecar transactions is
    /// dropped; otherwise its balance is unknown, and backs nothing, until the
    /// next update or validation.
    pub(crate) fn record_validated_balance(&mut self, sender: Address, balance: U256, epoch: u64) {
        self.usage.record_validated_balance(sender, balance, epoch);
        self.refresh_tip_backing(sender);
    }

    /// Returns a best-transactions iterator snapshot.
    ///
    /// Unlike the protocol pool, the sidecar never queues a transaction on its
    /// sender's balance, so a sender's coinbase tips rank its transactions only
    /// while its balance covers the summed `cost()` of all its sidecar
    /// transactions. That backing is kept on the transactions as costs and
    /// balances change (see `refresh_tip_backing`), so the snapshot costs the
    /// pending lanes, not every transaction.
    pub(crate) fn best_transactions<O>(
        &self,
        ordering: O,
        base_fee: u64,
    ) -> BestTwoDTransactions<T, O>
    where
        O: TransactionOrdering<Transaction = T>,
    {
        BestTwoDTransactions::new(
            &self.lanes,
            &self.nonce_free,
            self.protocol_nonce_lanes,
            ordering,
            base_fee,
        )
    }

    fn remove_hash(&mut self, hash: TxHash) -> Option<Arc<ValidPoolTransaction<T>>> {
        if let Some(transaction) = self.hashes.get(&hash)
            && let Some(replay_id) = transaction.transaction.eip8130_replay_id()
        {
            let transaction = self.nonce_free.remove(&replay_id)?;
            self.hashes.remove(&hash);
            self.usage.remove(&transaction);
            self.eviction_order.remove(&self.eviction_key(&transaction, true));
            self.refresh_tip_backing(transaction.sender());
            return Some(transaction);
        }
        let transaction = self.hashes.get(&hash)?;
        let nonce_key = self.lane_key(&transaction.transaction)?;
        let lane_id = (transaction.sender(), nonce_key);
        let nonce = transaction.nonce();
        let transaction = self.lanes.get_mut(&lane_id)?.remove(nonce)?;

        if self.lanes.get(&lane_id).is_some_and(|lane| lane.transactions.is_empty()) {
            self.lanes.remove(&lane_id);
        }
        self.hashes.remove(&hash);
        self.usage.remove(&transaction);
        self.refresh_lane_indexes(lane_id);
        self.refresh_tip_backing(lane_id.0);
        Some(transaction)
    }

    /// Re-derives whether `sender`'s balance backs its coinbase tips and, when
    /// that changed, records it on every live transaction of the sender.
    /// Called after each change to the sender's summed cost or balance, so
    /// [`Self::best_transactions`] never has to walk the pool for it.
    fn refresh_tip_backing(&mut self, sender: Address) {
        if !self.usage.per_sender.contains_key(&sender) {
            return;
        }
        let backed = self.usage.tip_backed(&sender);
        if self.usage.applied_backing.insert(sender, backed) == Some(backed) {
            return;
        }
        for hash in self.usage.sender_hashes.get(&sender).into_iter().flatten() {
            if let Some(transaction) = self.hashes.get(hash) {
                transaction.transaction.set_coinbase_tip_backed(backed);
            }
        }
    }
}

/// Snapshot iterator over the current best transactions of the EIP-8130 sidecar.
///
/// Each finite channel contributes its contiguous head and each nonce-free
/// transaction contributes an independent one-item candidate.
#[derive(Debug)]
pub(crate) struct BestTwoDTransactions<T: BasePooledTx, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    lanes: Vec<LaneIterator<T>>,
    /// Lane heads by priority. The priority names the transaction it was
    /// computed for, so an entry that no longer matches its lane's current
    /// member (the lane moved on, or was re-offered after a hold) is skipped.
    candidates: BinaryHeap<(BestTransactionPriority<O::PriorityValue>, usize)>,
    /// Only lanes with more than one member: invalidating or holding a lane
    /// after its single member was yielded has nothing left to act on.
    lane_indexes: HashMap<LaneId, usize>,
    /// Mirrors the pool's protocol-nonce lane mode for lane lookups.
    protocol_nonce_lanes: bool,
    ordering: O,
    base_fee: u64,
}

/// One lane's pending run as the snapshot walks it.
#[derive(Debug)]
struct LaneIterator<T: BasePooledTx> {
    id: LaneId,
    head: Arc<ValidPoolTransaction<T>>,
    /// Members after `head`, in nonce order. Empty for a one-member lane, so
    /// most lanes cost no allocation beyond the head's reference.
    tail: Vec<Arc<ValidPoolTransaction<T>>>,
    /// Members yielded so far: `0` means `head` is next, `n` means `tail[n - 1]`.
    index: usize,
    invalidated: bool,
    /// Held by the merge until its waiting head is yielded; see `MergeSource`.
    held: bool,
}

impl<T: BasePooledTx> LaneIterator<T> {
    const fn new(
        id: LaneId,
        head: Arc<ValidPoolTransaction<T>>,
        tail: Vec<Arc<ValidPoolTransaction<T>>>,
    ) -> Self {
        Self { id, head, tail, index: 0, invalidated: false, held: false }
    }

    fn current(&self) -> Option<&Arc<ValidPoolTransaction<T>>> {
        match self.index {
            0 => Some(&self.head),
            n => self.tail.get(n - 1),
        }
    }
}

impl<T: BasePooledTx, O> BestTwoDTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    fn new(
        lanes: &HashMap<LaneId, NonceLane<T>>,
        nonce_free: &B256Map<Arc<ValidPoolTransaction<T>>>,
        protocol_nonce_lanes: bool,
        ordering: O,
        base_fee: u64,
    ) -> Self {
        let mut snapshot = Vec::with_capacity(lanes.len() + nonce_free.len());
        snapshot.extend(lanes.iter().filter_map(|(id, lane)| {
            let mut pending = lane.consecutive_pending_transactions();
            let head = Arc::clone(pending.next()?);
            let mut tail = Vec::with_capacity(lane.consecutive_pending_len() - 1);
            tail.extend(pending.cloned());
            Some(LaneIterator::new(*id, head, tail))
        }));
        let lane_indexes = snapshot
            .iter()
            .enumerate()
            .filter(|(_, lane)| !lane.tail.is_empty())
            .map(|(index, lane)| (lane.id, index))
            .collect();
        snapshot.extend(nonce_free.values().map(|transaction| {
            let id = (transaction.sender(), Eip8130Constants::NONCE_KEY_MAX);
            LaneIterator::new(id, Arc::clone(transaction), Vec::new())
        }));
        let candidates = BinaryHeap::from(
            snapshot
                .iter()
                .enumerate()
                .map(|(index, lane)| {
                    (BestTransactionPriority::new(&ordering, &lane.head, base_fee), index)
                })
                .collect::<Vec<_>>(),
        );
        Self { lanes: snapshot, candidates, lane_indexes, protocol_nonce_lanes, ordering, base_fee }
    }

    fn push_lane_head(&mut self, index: usize) {
        let lane = &self.lanes[index];
        if lane.invalidated || lane.held {
            return;
        }
        if let Some(current) = lane.current() {
            let priority = BestTransactionPriority::new(&self.ordering, current, self.base_fee);
            self.candidates.push((priority, index));
        }
    }
}

impl<T: BasePooledTx, O> Iterator for BestTwoDTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (priority, best_index) = self.candidates.pop()?;
            let lane = &mut self.lanes[best_index];
            let Some(current) = lane.current() else { continue };
            if lane.invalidated || lane.held || current.hash() != priority.hash() {
                continue;
            }
            let transaction = Arc::clone(current);
            lane.index += 1;
            self.push_lane_head(best_index);
            return Some(transaction);
        }
    }
}

impl<T: BasePooledTx, O> BestTransactions for BestTwoDTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    /// Parks the rest of `transaction`'s lane. Callers pass a transaction this
    /// iterator yielded: a lane of one (every nonce-free transaction included)
    /// has nothing left once yielded, so it is not indexed and this is a no-op.
    fn mark_invalid(&mut self, transaction: &Self::Item, _kind: InvalidPoolTransactionError) {
        let Some(nonce_key) = transaction
            .transaction
            .eip8130_nonce_channel_key()
            .or_else(|| self.protocol_nonce_lanes.then_some(U256::ZERO))
            .filter(|_| !transaction.transaction.is_eip8130_nonce_free())
        else {
            return;
        };
        if let Some(index) = self.lane_indexes.get(&(transaction.sender(), nonce_key)).copied() {
            self.lanes[index].invalidated = true;
        }
    }

    fn no_updates(&mut self) {}

    fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
}

impl<T: BasePooledTx, O> MergeSource<T> for BestTwoDTransactions<T, O>
where
    O: TransactionOrdering<Transaction = T>,
{
    fn hold_lane(&mut self, lane_id: &LaneId) {
        if let Some(index) = self.lane_indexes.get(lane_id).copied() {
            self.lanes[index].held = true;
        }
    }

    fn release_lane(&mut self, lane_id: &LaneId) {
        if let Some(index) = self.lane_indexes.get(lane_id).copied()
            && self.lanes[index].held
        {
            self.lanes[index].held = false;
            self.push_lane_head(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Instant,
    };

    use alloy_consensus::{
        SignableTransaction, Transaction, TxEip1559,
        transaction::{Recovered, SignerRecoverable},
    };
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Bytes, TxKind};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        BasePooledTransaction as ConsensusPooledTransaction, BaseTxEnvelope, Eip8130Signed,
        TxEip8130,
    };
    use reth_transaction_pool::{PoolTransaction, PriceBumpConfig, Priority, TransactionOrigin};

    use super::*;
    use crate::{
        BaseOrdering, BasePooledTransaction, LimitClass, ValidityOperator, ValidityPredicate,
    };

    #[derive(Clone, Debug, Default)]
    struct CountingOrdering {
        priority_evaluations: Arc<AtomicUsize>,
    }

    impl TransactionOrdering for CountingOrdering {
        type PriorityValue = u128;
        type Transaction = BasePooledTransaction;

        fn priority(
            &self,
            transaction: &Self::Transaction,
            base_fee: u64,
        ) -> Priority<Self::PriorityValue> {
            self.priority_evaluations.fetch_add(1, Ordering::Relaxed);
            transaction.effective_tip_per_gas(base_fee).into()
        }
    }

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
        signed_channel_tx_with_tip(signer, nonce_key, nonce_sequence, 0, max_fee_per_gas)
    }

    fn signed_channel_tx_with_tip(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        signed_tx(signer, nonce_key, nonce_sequence, 0, max_priority_fee_per_gas, max_fee_per_gas)
    }

    fn signed_tx(
        signer: &PrivateKeySigner,
        nonce_key: U256,
        nonce_sequence: u64,
        valid_before: u64,
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key,
            nonce_sequence,
            valid_after: 0,
            valid_before,
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
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(pooled, signer.address()))
    }

    fn signed_nonce_free_tx(
        signer: &PrivateKeySigner,
        valid_before: u64,
        max_priority_fee_per_gas: u128,
        max_fee_per_gas: u128,
    ) -> BasePooledTransaction {
        signed_tx(
            signer,
            Eip8130Constants::NONCE_KEY_MAX,
            0,
            valid_before,
            max_priority_fee_per_gas,
            max_fee_per_gas,
        )
    }

    fn valid_pool_transaction(
        transaction: BasePooledTransaction,
    ) -> ValidPoolTransaction<BasePooledTransaction> {
        valid_pool_transaction_at(transaction, Instant::now())
    }

    fn valid_pool_transaction_at(
        transaction: BasePooledTransaction,
        timestamp: Instant,
    ) -> ValidPoolTransaction<BasePooledTransaction> {
        ValidPoolTransaction {
            transaction_id: TransactionId::new(0u64.into(), transaction.nonce()),
            transaction,
            propagate: true,
            timestamp,
            origin: TransactionOrigin::External,
            authority_ids: None,
        }
    }

    fn best_transaction_priority_evaluations(lane_count: usize) -> usize {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        for nonce_key in 1..=lane_count {
            let transaction = valid_pool_transaction(signed_channel_tx(
                &signer,
                U256::from(nonce_key),
                0,
                nonce_key as u128,
            ));
            pool.insert_validated(transaction, 0).unwrap();
        }

        let ordering = CountingOrdering::default();
        let priority_evaluations = Arc::clone(&ordering.priority_evaluations);
        let yielded = pool.best_transactions(ordering, 0).count();

        assert_eq!(yielded, lane_count);
        priority_evaluations.load(Ordering::Relaxed)
    }

    fn run_best_transactions_wall_clock(lane_count: usize) {
        let signer = signer();
        let transaction =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(1), 0, 1_000)));

        let setup_started = Instant::now();
        let lanes = (1..=lane_count)
            .map(|nonce_key| {
                (
                    (signer.address(), U256::from(nonce_key)),
                    NonceLane::new(
                        LaneCursor::at(0),
                        BTreeMap::from([(0, Arc::clone(&transaction))]),
                    ),
                )
            })
            .collect();
        let setup_elapsed = setup_started.elapsed();

        let snapshot_started = Instant::now();
        let best = BestTwoDTransactions::new(
            &lanes,
            &B256Map::default(),
            false,
            BaseOrdering::coinbase_tip(),
            0,
        );
        let snapshot_elapsed = snapshot_started.elapsed();

        let drain_started = Instant::now();
        let yielded = best.count();
        let drain_elapsed = drain_started.elapsed();

        eprintln!(
            "{lane_count:>6} lanes: setup={setup_elapsed:?}, snapshot={snapshot_elapsed:?}, drain={drain_elapsed:?}"
        );
        assert_eq!(yielded, lane_count);
    }

    #[test]
    fn channelized_transactions_with_same_sequence_can_coexist() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(1), 0, 1_000));
        let second = valid_pool_transaction(signed_channel_tx(&signer, U256::from(2), 0, 1_000));

        pool.insert_validated(first, 0).unwrap();
        pool.insert_validated(second, 0).unwrap();

        let (pending, queued) = pool.pending_and_queued_txn_count();
        assert_eq!(pending, 2);
        assert_eq!(queued, 0);
        assert_eq!(pool.all_transactions().len(), 2);
    }

    #[test]
    fn best_transactions_evaluates_each_lane_head_once() {
        const SMALL_LANE_COUNT: usize = 16;
        const LARGE_LANE_COUNT: usize = SMALL_LANE_COUNT * 2;

        let small_evaluations = best_transaction_priority_evaluations(SMALL_LANE_COUNT);
        let large_evaluations = best_transaction_priority_evaluations(LARGE_LANE_COUNT);

        assert_eq!(small_evaluations, SMALL_LANE_COUNT);
        assert_eq!(large_evaluations, LARGE_LANE_COUNT);
    }

    #[test]
    #[ignore = "wall-clock diagnostic; run explicitly in release mode with --ignored --nocapture"]
    fn best_transactions_wall_clock_1k_lanes() {
        run_best_transactions_wall_clock(1_000);
    }

    #[test]
    #[ignore = "wall-clock diagnostic; run explicitly in release mode with --ignored --nocapture"]
    fn best_transactions_wall_clock_10k_lanes() {
        run_best_transactions_wall_clock(10_000);
    }

    #[test]
    #[ignore = "wall-clock diagnostic; run explicitly in release mode with --ignored --nocapture"]
    fn best_transactions_wall_clock_100k_lanes() {
        run_best_transactions_wall_clock(100_000);
    }

    #[test]
    fn nonce_free_transactions_coexist_and_replace_atomically_by_replay_id() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let first = valid_pool_transaction(signed_nonce_free_tx(&signer, 1, 0, 1_000));
        let distinct = valid_pool_transaction(signed_nonce_free_tx(&signer, 2, 1, 1_000));
        pool.insert_validated(first, 0).unwrap();
        pool.insert_validated(distinct, 0).unwrap();
        assert_eq!(pool.pending_and_queued_txn_count(), (2, 0));

        let underpriced = valid_pool_transaction(signed_nonce_free_tx(&signer, 1, 0, 1_050));
        assert!(matches!(
            pool.insert_validated(underpriced, 0).unwrap_err().kind,
            PoolErrorKind::ReplacementUnderpriced
        ));
        let replacement = valid_pool_transaction(signed_nonce_free_tx(&signer, 1, 0, 1_250));
        assert!(pool.insert_validated(replacement, 0).unwrap().replaced.is_some());
        assert_eq!(pool.pending_and_queued_txn_count(), (2, 0));
    }

    #[test]
    fn validity_transaction_replacement_requires_the_price_bump() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let predicate = ValidityPredicate::Balance {
            address: Address::ZERO,
            op: ValidityOperator::Equal,
            value: U256::ZERO,
        };
        let original = signed_nonce_free_tx(&signer, 1, 10, 100)
            .with_validity_predicates(vec![predicate.clone()]);
        let original = valid_pool_transaction(original);
        let underpriced = signed_nonce_free_tx(&signer, 1, 0, 101)
            .with_validity_predicates(vec![predicate.clone()]);
        let replacement =
            signed_nonce_free_tx(&signer, 1, 11, 110).with_validity_predicates(vec![predicate]);

        pool.insert_validated(original, 0).unwrap();
        assert!(matches!(
            pool.insert_validated(valid_pool_transaction(underpriced), 0).unwrap_err().kind,
            PoolErrorKind::ReplacementUnderpriced
        ));
        assert!(
            pool.insert_validated(valid_pool_transaction(replacement), 0)
                .unwrap()
                .replaced
                .is_some()
        );
    }

    #[test]
    fn nonce_free_removal_and_iteration_are_independent() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let high = valid_pool_transaction(signed_nonce_free_tx(&signer, 1, 20, 1_000));
        let low = valid_pool_transaction(signed_nonce_free_tx(&signer, 2, 10, 1_000));
        let high_hash = *high.hash();
        let low_hash = *low.hash();
        pool.insert_validated(high, 0).unwrap();
        pool.insert_validated(low, 0).unwrap();

        let mut best = pool.best_transactions(BaseOrdering::coinbase_tip(), 0);
        let yielded = best.next().unwrap();
        assert_eq!(*yielded.hash(), high_hash);
        best.mark_invalid(&yielded, InvalidPoolTransactionError::Underpriced);
        assert_eq!(best.next().map(|tx| *tx.hash()), Some(low_hash));

        assert_eq!(pool.remove_transactions_and_descendants(&[high_hash]).len(), 1);
        assert!(pool.get(&low_hash).is_some());
        assert_eq!(pool.prune_mined(&[low_hash]).removed.len(), 1);
        assert!(pool.all_transactions().is_empty());
    }

    #[test]
    fn nonce_free_expiry_removes_due_transactions_and_keeps_future_entries() {
        // Millisecond-scale bounds (>= TIMESTAMP_MS_THRESHOLD) so normalization
        // is a no-op and `valid_before` is compared directly against the
        // millisecond `now`.
        const NOW_MS: u64 = 1_700_000_000_000;
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let due = valid_pool_transaction(signed_nonce_free_tx(&signer, NOW_MS, 20, 1_000));
        let future = valid_pool_transaction(signed_nonce_free_tx(&signer, NOW_MS + 1, 10, 1_000));
        let due_hash = *due.hash();
        let future_hash = *future.hash();
        pool.insert_validated(due, 0).unwrap();
        pool.insert_validated(future, 0).unwrap();

        let removed = pool.remove_expired_nonce_free(NOW_MS);

        assert_eq!(removed.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(), vec![due_hash]);
        assert!(pool.get(&due_hash).is_none());
        assert!(pool.get(&future_hash).is_some());
        assert_eq!(pool.pending_and_queued_txn_count(), (1, 0));
    }

    #[test]
    fn remove_by_sender_cleans_nonce_free_and_finite_channel_indexes() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let nonce_free = valid_pool_transaction(signed_nonce_free_tx(&signer, 10, 20, 1_000));
        let channel = valid_pool_transaction(signed_channel_tx(&signer, U256::from(7), 0, 1_000));
        let nonce_free_hash = *nonce_free.hash();
        let channel_hash = *channel.hash();
        pool.insert_validated(nonce_free, 0).unwrap();
        pool.insert_validated(channel, 0).unwrap();

        let removed = pool.remove_transactions_by_sender(signer.address());

        assert_eq!(removed.len(), 2);
        assert!(pool.get(&nonce_free_hash).is_none());
        assert!(pool.get(&channel_hash).is_none());
        assert!(pool.all_transactions().is_empty());
        assert!(pool.unique_senders().is_empty());
    }

    #[test]
    fn same_channel_sequence_replacement_is_lane_local() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let original = valid_pool_transaction(signed_channel_tx(&signer, U256::from(7), 0, 1_000));
        let replacement =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(7), 0, 1_250));
        let original_hash = *original.hash();
        let replacement_hash = *replacement.hash();

        pool.insert_validated(original, 0).unwrap();
        let outcome = pool.insert_validated(replacement, 0).unwrap();

        assert_eq!(
            outcome.replaced.as_ref().map(|transaction| *transaction.hash()),
            Some(original_hash)
        );
        assert!(pool.get(&original_hash).is_none());
        assert!(pool.get(&replacement_hash).is_some());
        assert_eq!(pool.all_transactions().len(), 1);
    }

    #[test]
    fn pruning_mined_head_promotes_next_sequence_in_lane() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let head = valid_pool_transaction(signed_channel_tx(&signer, U256::from(3), 0, 1_000));
        let head_hash = *head.hash();
        let queued = valid_pool_transaction(signed_channel_tx(&signer, U256::from(3), 1, 900));
        let queued_hash = *queued.hash();

        pool.insert_validated(head, 0).unwrap();
        pool.insert_validated(queued, 0).unwrap();

        let (pending, queued_count) = pool.pending_and_queued_txn_count();
        assert_eq!((pending, queued_count), (2, 0));
        assert_eq!(
            pool.pending_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![head_hash, queued_hash]
        );

        let outcome = pool.prune_mined(&[head_hash]);
        assert_eq!(
            outcome.removed.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![head_hash]
        );

        let (pending, queued_count) = pool.pending_and_queued_txn_count();
        assert_eq!((pending, queued_count), (1, 0));
        assert_eq!(
            pool.pending_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![queued_hash]
        );
    }

    fn hashes(transactions: &[Arc<ValidPoolTransaction<BasePooledTransaction>>]) -> Vec<TxHash> {
        transactions.iter().map(|transaction| *transaction.hash()).collect()
    }

    #[test]
    fn higher_state_nonce_discards_transactions_below_the_cursor() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let key = U256::from(4);
        let stale: Vec<_> = (0..2)
            .map(|nonce| valid_pool_transaction(signed_channel_tx(&signer, key, nonce, 1_000)))
            .collect();
        let stale_hashes: Vec<_> = stale.iter().map(|transaction| *transaction.hash()).collect();
        for transaction in stale {
            pool.insert_validated(transaction, 0).unwrap();
        }

        let next = valid_pool_transaction(signed_channel_tx(&signer, key, 2, 1_000));
        let outcome = pool.insert_validated(next, 2).unwrap();

        assert_eq!(hashes(&outcome.anchor.discarded), stale_hashes);
        assert!(stale_hashes.iter().all(|hash| !pool.contains(hash)));
        assert_eq!(pool.all_hashes(), vec![outcome.outcome.hash]);
    }

    #[test]
    fn rejected_insert_leaves_the_lane_cursor_unchanged() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let key = U256::from(4);
        let head = valid_pool_transaction(signed_channel_tx(&signer, key, 0, 1_000));
        let head_hash = *head.hash();
        pool.insert_validated(head, 0).unwrap();

        let too_low = valid_pool_transaction(signed_channel_tx(&signer, key, 1, 1_000));
        assert!(pool.insert_validated(too_low, 2).is_err());
        assert!(pool.contains(&head_hash));
    }

    #[test]
    fn mined_nonce_past_a_gap_advances_the_cursor() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let key = U256::from(5);
        let [head, mined, next] = [0, 2, 3]
            .map(|nonce| valid_pool_transaction(signed_channel_tx(&signer, key, nonce, 1_000)));
        let (head_hash, mined_hash, next_hash) = (*head.hash(), *mined.hash(), *next.hash());
        for transaction in [head, mined, next] {
            pool.insert_validated(transaction, 0).unwrap();
        }

        // The block also mines nonce 1, whose hash this pool never held.
        let outcome = pool.prune_mined(&[head_hash, TxHash::repeat_byte(0x11), mined_hash]);

        assert_eq!(hashes(&outcome.removed), vec![head_hash, mined_hash]);
        assert_eq!(hashes(&outcome.lanes.promoted), vec![next_hash]);
        assert_eq!(hashes(&pool.pending_transactions()), vec![next_hash]);
    }

    #[test]
    fn mined_nonce_discards_a_different_hash_at_that_nonce() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let key = U256::from(6);
        let [mined, other, next, successor] = [0, 1, 2, 3]
            .map(|nonce| valid_pool_transaction(signed_channel_tx(&signer, key, nonce, 1_000)));
        let (mined_hash, other_hash, next_hash) = (*mined.hash(), *other.hash(), *next.hash());
        for transaction in [mined, other, next, successor] {
            pool.insert_validated(transaction, 0).unwrap();
        }

        // The block mines nonces 0 to 2, nonce 1 under a hash this pool does not hold.
        let outcome = pool.prune_mined(&[mined_hash, next_hash]);

        assert_eq!(hashes(&outcome.removed), vec![mined_hash, next_hash]);
        assert_eq!(hashes(&outcome.lanes.discarded), vec![other_hash]);
        assert!(!pool.contains(&other_hash));
    }

    #[test]
    fn lower_state_nonce_reports_transactions_left_behind_a_gap_as_queued() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let key = U256::from(7);
        let pending: Vec<_> = (2..4)
            .map(|nonce| valid_pool_transaction(signed_channel_tx(&signer, key, nonce, 1_000)))
            .collect();
        let pending_hashes: Vec<_> =
            pending.iter().map(|transaction| *transaction.hash()).collect();
        for transaction in pending {
            pool.insert_validated(transaction, 2).unwrap();
        }

        // A reorg rewinds the channel to 1; nonce 1 is not pooled.
        let later = valid_pool_transaction(signed_channel_tx(&signer, key, 5, 1_000));
        let outcome = pool.insert_validated(later, 1).unwrap();

        assert_eq!(hashes(&outcome.anchor.queued), pending_hashes);
        assert!(outcome.anchor.discarded.is_empty());
        assert!(pool.pending_transactions().is_empty());
    }

    #[test]
    fn contiguous_lane_counts_full_run_as_pending() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(9), 0, 1_000));
        let second = valid_pool_transaction(signed_channel_tx(&signer, U256::from(9), 1, 900));
        let third = valid_pool_transaction(signed_channel_tx(&signer, U256::from(9), 2, 800));
        let gap = valid_pool_transaction(signed_channel_tx(&signer, U256::from(9), 4, 700));

        let first_hash = *first.hash();
        let second_hash = *second.hash();
        let third_hash = *third.hash();
        let gap_hash = *gap.hash();

        pool.insert_validated(first, 0).unwrap();
        pool.insert_validated(second, 0).unwrap();
        pool.insert_validated(third, 0).unwrap();
        pool.insert_validated(gap, 0).unwrap();

        let (pending, queued) = pool.pending_and_queued_txn_count();
        assert_eq!((pending, queued), (3, 1));
        assert_eq!(
            pool.pending_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![first_hash, second_hash, third_hash]
        );
        assert_eq!(
            pool.queued_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![gap_hash]
        );
    }

    #[test]
    fn queued_transactions_ignore_stale_nonces_below_lane_head() {
        let signer = signer();
        let stale =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(15), 3, 1_000)));
        let first_pending =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(15), 5, 900)));
        let second_pending =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(15), 6, 800)));
        let queued =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(15), 10, 700)));

        let lane = NonceLane::new(
            LaneCursor::at(5),
            BTreeMap::from([
                (3, Arc::clone(&stale)),
                (5, Arc::clone(&first_pending)),
                (6, Arc::clone(&second_pending)),
                (10, Arc::clone(&queued)),
            ]),
        );

        assert_eq!(
            lane.queued_transactions().map(|transaction| *transaction.hash()).collect::<Vec<_>>(),
            vec![*queued.hash()]
        );
    }

    #[test]
    fn consecutive_pending_handles_u64_max_nonce_without_overflow() {
        let signer = signer();
        let transaction = Arc::new(valid_pool_transaction(signed_channel_tx(
            &signer,
            U256::from(16),
            u64::MAX,
            1_000,
        )));

        let lane = NonceLane::new(
            LaneCursor::at(u64::MAX),
            BTreeMap::from([(u64::MAX, Arc::clone(&transaction))]),
        );

        assert_eq!(
            lane.consecutive_pending_transactions()
                .map(|transaction| *transaction.hash())
                .collect::<Vec<_>>(),
            vec![*transaction.hash()]
        );
        assert!(lane.queued_transactions().next().is_none());
    }

    #[test]
    fn all_transactions_and_hashes_skip_stale_entries_below_lane_head() {
        let signer = signer();
        let stale =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(23), 3, 1_000)));
        let pending =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(23), 5, 900)));
        let queued =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(23), 7, 800)));
        let lane_id = (signer.address(), U256::from(23));

        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        pool.hashes.insert(*stale.hash(), Arc::clone(&stale));
        pool.hashes.insert(*pending.hash(), Arc::clone(&pending));
        pool.hashes.insert(*queued.hash(), Arc::clone(&queued));
        pool.lanes.insert(
            lane_id,
            NonceLane::new(
                LaneCursor::at(5),
                BTreeMap::from([
                    (3, Arc::clone(&stale)),
                    (5, Arc::clone(&pending)),
                    (7, Arc::clone(&queued)),
                ]),
            ),
        );

        assert_eq!(
            pool.all_transactions().into_iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![*pending.hash(), *queued.hash()]
        );
        assert_eq!(pool.all_hashes(), vec![*pending.hash(), *queued.hash()]);
        assert_eq!(
            pool.transactions_by_sender(signer.address())
                .into_iter()
                .map(|tx| *tx.hash())
                .collect::<Vec<_>>(),
            vec![*pending.hash(), *queued.hash()]
        );
        assert_eq!(pool.pending_and_queued_txn_count(), (1, 1));
        assert_eq!(pool.unique_senders(), HashSet::from([signer.address()]));
    }

    #[test]
    fn best_transactions_snapshot_handles_u64_max_nonce_without_wrapping() {
        let signer = signer();
        let transaction = Arc::new(valid_pool_transaction(signed_channel_tx(
            &signer,
            U256::from(17),
            u64::MAX,
            1_000,
        )));
        let lane_id = (signer.address(), U256::from(17));
        let lanes: HashMap<_, _> = [(
            lane_id,
            NonceLane::new(
                LaneCursor::at(u64::MAX),
                BTreeMap::from([(u64::MAX, Arc::clone(&transaction))]),
            ),
        )]
        .into_iter()
        .collect();

        let mut best = BestTwoDTransactions::new(
            &lanes,
            &B256Map::default(),
            false,
            BaseOrdering::coinbase_tip(),
            0,
        );
        assert_eq!(best.next().map(|transaction| *transaction.hash()), Some(*transaction.hash()));
        assert!(best.next().is_none());
    }

    #[test]
    fn insert_validated_classifies_u64_max_head_as_pending_without_overflow() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let transaction =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(18), u64::MAX, 1_000));

        let lane_id = (signer.address(), U256::from(18));
        pool.lanes.insert(lane_id, NonceLane::new(LaneCursor::at(u64::MAX), BTreeMap::new()));

        let outcome = pool.insert_validated(transaction, u64::MAX).unwrap();

        assert!(matches!(outcome.outcome.state, AddedTransactionState::Pending));
    }

    #[test]
    fn prune_mined_does_not_wrap_lane_head_after_u64_max() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let head = Arc::new(valid_pool_transaction(signed_channel_tx(
            &signer,
            U256::from(19),
            u64::MAX,
            1_000,
        )));
        let head_hash = *head.hash();
        let lane_id = (signer.address(), U256::from(19));

        pool.hashes.insert(head_hash, Arc::clone(&head));
        pool.lanes.insert(
            lane_id,
            NonceLane::new(LaneCursor::at(u64::MAX), BTreeMap::from([(u64::MAX, head)])),
        );

        let removed = pool.prune_mined(&[head_hash]).removed;

        // Mining the last possible nonce empties the lane; its cursor never
        // wraps back to a nonce that could accept a replay.
        assert_eq!(removed.len(), 1);
        assert!(pool.lanes.get(&lane_id).is_none_or(|lane| lane.next_nonce == u64::MAX));
    }

    #[test]
    fn gap_fill_reports_newly_promoted_transactions() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(13), 0, 1_000));
        let gap = valid_pool_transaction(signed_channel_tx(&signer, U256::from(13), 2, 800));
        let middle = valid_pool_transaction(signed_channel_tx(&signer, U256::from(13), 1, 900));
        let gap_hash = *gap.hash();

        pool.insert_validated(first, 0).unwrap();
        pool.insert_validated(gap, 0).unwrap();

        let outcome = pool.insert_validated(middle, 0).unwrap();

        assert_eq!(
            outcome.promoted.iter().map(|transaction| *transaction.hash()).collect::<Vec<_>>(),
            vec![gap_hash]
        );
    }

    #[test]
    fn pruning_mined_sorts_hashes_within_lane() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first = valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 0, 1_000));
        let first_hash = *first.hash();
        let second = valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 1, 900));
        let second_hash = *second.hash();
        let third = valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 2, 800));
        let third_hash = *third.hash();
        let queued = valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 4, 700));

        pool.insert_validated(first, 0).unwrap();
        pool.insert_validated(second, 0).unwrap();
        pool.insert_validated(third, 0).unwrap();
        pool.insert_validated(queued, 0).unwrap();

        pool.prune_mined(&[third_hash, first_hash, second_hash]);

        let replacement =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(11), 2, 850));
        let error = pool.insert_validated(replacement, 3).unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::InvalidTransaction(_)));
    }

    #[test]
    fn inserting_non_channelized_transaction_returns_error() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let non_channelized =
            valid_pool_transaction(signed_channel_tx(&signer, U256::ZERO, 0, 1_000));

        let error = pool.insert_validated(non_channelized, 0).unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::Other(_)));
    }

    #[test]
    fn mark_invalid_only_invalidates_matching_lane() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let first_lane_head =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(21), 0, 1_000));
        let first_lane_head_hash = *first_lane_head.hash();
        let first_lane_next =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(21), 1, 900));
        let second_lane_head =
            valid_pool_transaction(signed_channel_tx(&signer, U256::from(22), 0, 950));
        let second_lane_head_hash = *second_lane_head.hash();

        pool.insert_validated(first_lane_head, 0).unwrap();
        pool.insert_validated(first_lane_next, 0).unwrap();
        pool.insert_validated(second_lane_head, 0).unwrap();

        let lane_to_invalidate = pool.get(&first_lane_head_hash).unwrap();
        let mut best = pool.best_transactions(BaseOrdering::coinbase_tip(), 0);
        best.mark_invalid(
            &lane_to_invalidate,
            InvalidPoolTransactionError::Consensus(InvalidTransactionError::TxTypeNotSupported),
        );

        let yielded_hashes: Vec<_> = best.map(|transaction| *transaction.hash()).collect();
        assert_eq!(yielded_hashes.len(), 1);
        assert_eq!(yielded_hashes[0], second_lane_head_hash);
    }

    #[test]
    fn a_queued_member_at_u64_max_counts_as_queued() {
        let signer = signer();
        let head =
            Arc::new(valid_pool_transaction(signed_channel_tx(&signer, U256::from(43), 0, 1_000)));
        let last = Arc::new(valid_pool_transaction(signed_channel_tx(
            &signer,
            U256::from(43),
            u64::MAX,
            1_000,
        )));
        let lane = NonceLane::new(
            LaneCursor::at(0),
            BTreeMap::from([(0, Arc::clone(&head)), (u64::MAX, Arc::clone(&last))]),
        );

        assert_eq!(lane.consecutive_pending_len(), 1);
        assert_eq!(lane.queued_len(), 1);
        assert_eq!(
            lane.queued_transactions().map(|transaction| *transaction.hash()).collect::<Vec<_>>(),
            vec![*last.hash()]
        );
    }

    #[test]
    fn a_held_lane_is_skipped_until_released() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let held_lane = (signer.address(), U256::from(41));
        let rich: Vec<_> = (0..3)
            .map(|sequence| {
                valid_pool_transaction(signed_channel_tx(&signer, U256::from(41), sequence, 1_000))
            })
            .collect();
        let poor = valid_pool_transaction(signed_channel_tx(&signer, U256::from(42), 0, 100));
        let rich_hashes: Vec<_> = rich.iter().map(|tx| *tx.hash()).collect();
        let poor_hash = *poor.hash();
        for transaction in rich {
            pool.insert_validated(transaction, 0).unwrap();
        }
        pool.insert_validated(poor, 0).unwrap();

        let mut best = pool.best_transactions(BaseOrdering::coinbase_tip(), 0);
        assert_eq!(best.next().map(|tx| *tx.hash()), Some(rich_hashes[0]));
        // Holding the lane after its head was pulled leaves the rest of it
        // alone: the other lane is offered instead of the lane's next member.
        best.hold_lane(&held_lane);
        assert_eq!(best.next().map(|tx| *tx.hash()), Some(poor_hash));
        assert!(best.next().is_none());
        // Releasing picks the lane up where it stopped, in order.
        best.release_lane(&held_lane);
        assert_eq!(
            best.map(|tx| *tx.hash()).collect::<Vec<_>>(),
            vec![rich_hashes[1], rich_hashes[2]]
        );
    }

    #[test]
    fn best_transactions_uses_effective_tip_across_sidecar_lanes() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();

        let low_tip_high_cap =
            valid_pool_transaction(signed_channel_tx_with_tip(&signer, U256::from(31), 0, 1, 100));
        let high_tip_lower_cap =
            valid_pool_transaction(signed_channel_tx_with_tip(&signer, U256::from(32), 0, 50, 50));
        let high_tip_hash = *high_tip_lower_cap.hash();

        pool.insert_validated(low_tip_high_cap, 0).unwrap();
        pool.insert_validated(high_tip_lower_cap, 0).unwrap();

        let mut best = pool.best_transactions(BaseOrdering::coinbase_tip(), 10);
        assert_eq!(best.next().map(|transaction| *transaction.hash()), Some(high_tip_hash));
    }

    #[test]
    fn equal_priority_prefers_earlier_submission_timestamp_across_sidecar_lanes() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let signer = signer();
        let now = Instant::now();

        let older = valid_pool_transaction_at(
            signed_channel_tx_with_tip(&signer, U256::from(41), 0, 10, 100),
            now,
        );
        let older_hash = *older.hash();
        let newer = valid_pool_transaction_at(
            signed_channel_tx_with_tip(&signer, U256::from(42), 0, 10, 100),
            now + std::time::Duration::from_secs(1),
        );

        pool.insert_validated(older, 0).unwrap();
        pool.insert_validated(newer, 0).unwrap();

        let mut best = pool.best_transactions(BaseOrdering::coinbase_tip(), 10);
        assert_eq!(best.next().map(|transaction| *transaction.hash()), Some(older_hash));
    }

    #[test]
    fn sender_slot_cap_counts_fresh_lane_heads() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default())
            .with_limits(SidecarLimits { max_account_slots: 2, ..SidecarLimits::unlimited() });
        let signer = signer();
        for nonce_key in 1..=2 {
            let queued = signed_channel_tx(&signer, U256::from(nonce_key), 1, 1_000);
            pool.insert_validated(valid_pool_transaction(queued), 0).unwrap();
        }

        let over_cap = signed_channel_tx(&signer, U256::from(3), 1, 1_000);
        let error = pool.insert_validated(valid_pool_transaction(over_cap), 0).unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::SpammerExceededCapacity(_)));
        let fresh_lane_head = signed_channel_tx(&signer, U256::from(3), 0, 1_000);
        let error = pool.insert_validated(valid_pool_transaction(fresh_lane_head), 0).unwrap_err();
        assert!(matches!(error.kind, PoolErrorKind::SpammerExceededCapacity(_)));

        let gap_filling_head = signed_channel_tx(&signer, U256::from(1), 0, 1_000);
        pool.insert_validated(valid_pool_transaction(gap_filling_head), 0).unwrap();
        let replacement = signed_channel_tx(&signer, U256::from(1), 1, 2_000);
        let outcome = pool.insert_validated(valid_pool_transaction(replacement), 0).unwrap();
        assert!(outcome.replaced.is_some());
    }

    #[test]
    fn slots_are_released_when_a_cursor_advance_discards_transactions() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default())
            .with_limits(SidecarLimits { max_account_slots: 2, ..SidecarLimits::unlimited() });
        let signer = signer();
        let nonce_key = U256::from(1);
        for sequence in 0..2 {
            let transaction = signed_channel_tx(&signer, nonce_key, sequence, 1_000);
            pool.insert_validated(valid_pool_transaction(transaction), 0).unwrap();
        }

        let update = pool.advance_lane_nonce((signer.address(), nonce_key), 2);
        assert_eq!(update.discarded.len(), 2);

        for sequence in 2..4 {
            let transaction = signed_channel_tx(&signer, nonce_key, sequence, 1_000);
            pool.insert_validated(valid_pool_transaction(transaction), 2).unwrap();
        }
    }

    #[test]
    fn eviction_takes_the_cheapest_lane_tail_without_opening_a_gap() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default()).with_limits(SidecarLimits {
            capacity: SubPoolLimit::new(2, usize::MAX),
            ..SidecarLimits::unlimited()
        });
        let lane_signer = signer();
        let cheap_head =
            valid_pool_transaction(signed_channel_tx(&lane_signer, U256::from(1), 0, 50));
        let cheap_head_hash = *cheap_head.hash();
        let rich_tail =
            valid_pool_transaction(signed_channel_tx(&lane_signer, U256::from(1), 1, 5_000));
        let rich_tail_hash = *rich_tail.hash();
        let other = valid_pool_transaction(signed_channel_tx(&signer(), U256::from(1), 0, 1_000));
        let other_hash = *other.hash();
        pool.insert_validated(cheap_head, 0).unwrap();
        pool.insert_validated(rich_tail, 0).unwrap();
        pool.insert_validated(other, 0).unwrap();

        let evicted: Vec<_> =
            pool.evict_over_capacity().iter().map(|transaction| *transaction.hash()).collect();

        assert_eq!(evicted, vec![other_hash]);
        assert!(pool.contains(&cheap_head_hash));
        assert!(pool.contains(&rich_tail_hash));
        assert!(pool.evict_over_capacity().is_empty());
    }

    #[test]
    fn full_sidecar_of_queued_spam_still_admits_a_cheaper_pending_head() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default()).with_limits(SidecarLimits {
            capacity: SubPoolLimit::new(2, usize::MAX),
            ..SidecarLimits::unlimited()
        });
        let spam: Vec<_> = (0..2)
            .map(|_| {
                let gapped = valid_pool_transaction(signed_channel_tx(
                    &signer(),
                    U256::from(1),
                    1,
                    1_000_000,
                ));
                let hash = *gapped.hash();
                pool.insert_validated(gapped, 0).unwrap();
                hash
            })
            .collect();
        assert_eq!(pool.pending_and_queued_txn_count(), (0, 2));
        let honest = valid_pool_transaction(signed_channel_tx(&signer(), U256::from(1), 0, 100));
        let honest_hash = *honest.hash();
        pool.insert_validated(honest, 0).unwrap();

        let evicted: Vec<_> =
            pool.evict_over_capacity().iter().map(|transaction| *transaction.hash()).collect();

        assert_eq!(evicted.len(), 1);
        assert!(spam.contains(&evicted[0]), "a queued transaction is evicted first");
        assert!(pool.contains(&honest_hash), "the pending head survives");
    }

    fn capped_pool(capacity: usize) -> TwoDNoncePool<BasePooledTransaction> {
        TwoDNoncePool::new(PriceBumpConfig::default()).with_limits(SidecarLimits {
            capacity: SubPoolLimit::new(capacity, usize::MAX),
            ..SidecarLimits::unlimited()
        })
    }

    fn evicted_hashes(pool: &mut TwoDNoncePool<BasePooledTransaction>) -> Vec<TxHash> {
        pool.evict_over_capacity().iter().map(|transaction| *transaction.hash()).collect()
    }

    #[test]
    fn filling_a_gap_ranks_the_lane_tail_as_pending() {
        let mut pool = capped_pool(2);
        let lane_signer = signer();
        let gapped =
            valid_pool_transaction(signed_channel_tx(&lane_signer, U256::from(1), 1, 1_000_000));
        let gapped_hash = *gapped.hash();
        let cheap = valid_pool_transaction(signed_channel_tx(&signer(), U256::from(1), 0, 100));
        let cheap_hash = *cheap.hash();
        pool.insert_validated(gapped, 0).unwrap();
        pool.insert_validated(cheap, 0).unwrap();
        pool.insert_validated(
            valid_pool_transaction(signed_channel_tx(&lane_signer, U256::from(1), 0, 2_000)),
            0,
        )
        .unwrap();

        assert_eq!(evicted_hashes(&mut pool), vec![cheap_hash]);
        assert!(pool.contains(&gapped_hash));
    }

    #[test]
    fn moving_a_cursor_behind_a_gap_ranks_the_lane_tail_as_queued() {
        let mut pool = capped_pool(2);
        let lane_signer = signer();
        let rich =
            valid_pool_transaction(signed_channel_tx(&lane_signer, U256::from(1), 5, 1_000_000));
        let rich_hash = *rich.hash();
        pool.insert_validated(rich, 5).unwrap();
        pool.set_lane_nonce((lane_signer.address(), U256::from(1)), 4);
        let cheap = valid_pool_transaction(signed_channel_tx(&signer(), U256::from(1), 0, 100));
        let cheap_hash = *cheap.hash();
        pool.insert_validated(cheap, 0).unwrap();
        pool.insert_validated(
            valid_pool_transaction(signed_channel_tx(&signer(), U256::from(1), 0, 200)),
            0,
        )
        .unwrap();

        assert_eq!(evicted_hashes(&mut pool), vec![rich_hash]);
        assert!(pool.contains(&cheap_hash));
    }

    #[test]
    fn evicting_a_lane_tail_exposes_the_next_one() {
        let mut pool = capped_pool(0);
        let lane_signer = signer();
        let head = valid_pool_transaction(signed_channel_tx(&lane_signer, U256::from(1), 0, 100));
        let head_hash = *head.hash();
        let tail = valid_pool_transaction(signed_channel_tx(&lane_signer, U256::from(1), 1, 100));
        let tail_hash = *tail.hash();
        pool.insert_validated(head, 0).unwrap();
        pool.insert_validated(tail, 0).unwrap();

        assert_eq!(evicted_hashes(&mut pool), vec![tail_hash, head_hash]);
        assert_eq!(pool.pending_and_queued_txn_count(), (0, 0));
    }

    #[test]
    fn nonce_free_eviction_follows_replacement_and_removal() {
        let mut pool = capped_pool(0);
        let signer = signer();
        let replaced = valid_pool_transaction(signed_nonce_free_tx(&signer, 1, 0, 100));
        let replacement = valid_pool_transaction(signed_nonce_free_tx(&signer, 1, 0, 5_000));
        let replacement_hash = *replacement.hash();
        let removed = valid_pool_transaction(signed_nonce_free_tx(&signer, 2, 0, 200));
        let removed_hash = *removed.hash();
        let cheapest = valid_pool_transaction(signed_nonce_free_tx(&signer, 3, 0, 300));
        let cheapest_hash = *cheapest.hash();
        let richest = valid_pool_transaction(signed_nonce_free_tx(&signer, 4, 0, 9_000));
        let richest_hash = *richest.hash();
        for transaction in [replaced, replacement, removed, cheapest, richest] {
            pool.insert_validated(transaction, 0).unwrap();
        }
        assert_eq!(pool.remove_transactions(&[removed_hash]).len(), 1);

        assert_eq!(evicted_hashes(&mut pool), vec![cheapest_hash, replacement_hash, richest_hash]);
    }

    fn sponsored_by(
        transaction: BasePooledTransaction,
        payer: Address,
    ) -> ValidPoolTransaction<BasePooledTransaction> {
        transaction.set_limit_class(LimitClass {
            sender: transaction.sender(),
            payer,
            classification_generation: 0,
            sender_locked: false,
            payer_locked: false,
            payer_trusted: false,
            payer_allowlisted: false,
            payer_balance: U256::MAX,
            max_cost: U256::ZERO,
        });
        valid_pool_transaction(transaction)
    }

    #[test]
    fn sender_ids_are_released_when_transactions_are_removed_or_mined() {
        const NOW_MS: u64 = 1_700_000_000_000;
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let [removed, mined, expired] = std::array::from_fn(|_| signer());
        let payer = Address::repeat_byte(0xA5);
        pool.insert_validated(
            sponsored_by(signed_channel_tx(&removed, U256::from(1), 0, 1_000), payer),
            0,
        )
        .unwrap();
        let mined_tx = valid_pool_transaction(signed_channel_tx(&mined, U256::from(2), 0, 1_000));
        let mined_hash = *mined_tx.hash();
        pool.insert_validated(mined_tx, 0).unwrap();
        pool.insert_validated(
            valid_pool_transaction(signed_nonce_free_tx(&expired, NOW_MS, 0, 1_000)),
            0,
        )
        .unwrap();
        assert_eq!(pool.usage.sender_ids.len(), 4, "three senders and one payer");

        pool.remove_transactions_by_sender(removed.address());
        assert_eq!(pool.usage.sender_ids.len(), 2, "the sender and its payer are released");
        pool.prune_mined(&[mined_hash]);
        pool.remove_expired_nonce_free(NOW_MS);

        assert!(pool.all_transactions().is_empty());
        assert_eq!(pool.usage.sender_ids.len(), 0);
    }

    #[test]
    fn sender_ids_are_released_when_transactions_are_replaced_or_evicted() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default()).with_limits(SidecarLimits {
            capacity: SubPoolLimit::new(1, usize::MAX),
            ..SidecarLimits::unlimited()
        });
        let [sender, evicted] = std::array::from_fn(|_| signer());
        let [first_payer, second_payer] = [Address::repeat_byte(0xA1), Address::repeat_byte(0xA2)];
        pool.insert_validated(
            sponsored_by(signed_channel_tx(&sender, U256::from(1), 0, 1_000), first_payer),
            0,
        )
        .unwrap();
        pool.insert_validated(
            sponsored_by(signed_channel_tx(&sender, U256::from(1), 0, 2_000), second_payer),
            0,
        )
        .unwrap();
        assert_eq!(pool.usage.sender_ids.len(), 2, "the replaced transaction's payer is released");

        let underpriced = signed_channel_tx(&evicted, U256::from(1), 0, 1);
        pool.insert_validated(valid_pool_transaction(underpriced), 0).unwrap();
        pool.evict_over_capacity();
        assert_eq!(pool.usage.sender_ids.len(), 2, "the evicted sender is released");

        pool.set_lane_nonce((sender.address(), U256::from(1)), 1);
        assert!(pool.all_transactions().is_empty());
        assert_eq!(
            pool.usage.sender_ids.len(),
            0,
            "a transaction below the lane cursor is released"
        );
    }

    #[test]
    fn removal_releases_sender_ids_and_tip_backing_together() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let sender = signer();
        let payer = Address::repeat_byte(0xA5);
        let self_paid = valid_pool_transaction(signed_channel_tx(&sender, U256::from(1), 0, 1_000));
        let self_paid_hash = *self_paid.hash();
        let self_paid_cost = *self_paid.transaction.cost();
        pool.insert_validated(self_paid, 0).unwrap();
        let sponsored = sponsored_by(signed_channel_tx(&sender, U256::from(2), 0, 1_000), payer);
        let sponsored_hash = *sponsored.hash();
        pool.insert_validated(sponsored, 0).unwrap();
        pool.apply_balance_updates([(sender.address(), self_paid_cost - U256::from(1))]);
        assert!(!pool.usage.tip_backed(&sender.address()));

        pool.remove_transactions(&[self_paid_hash]);
        assert!(pool.usage.tip_backed(&sender.address()), "the removed cost is released");
        assert_eq!(pool.usage.sender_ids.len(), 2, "the sponsored transaction keeps both ids");

        pool.remove_transactions(&[sponsored_hash]);
        assert_eq!(pool.usage.sender_ids.len(), 0);
        let returning = valid_pool_transaction(signed_channel_tx(&sender, U256::from(1), 0, 1_000));
        pool.insert_validated(returning, 0).unwrap();
        assert!(
            !pool.usage.tip_backed(&sender.address()),
            "a returning sender does not inherit its released balance"
        );
    }

    #[test]
    fn rejected_insert_allocates_no_sender_id() {
        let mut pool = TwoDNoncePool::new(PriceBumpConfig::default());
        let [existing, rejected] = std::array::from_fn(|_| signer());
        pool.insert_validated(
            valid_pool_transaction(signed_channel_tx(&existing, U256::from(1), 0, 1_000)),
            0,
        )
        .unwrap();

        let below_cursor = signed_channel_tx(&rejected, U256::from(1), 0, 1_000);
        assert!(pool.insert_validated(valid_pool_transaction(below_cursor), 1).is_err());
        let underpriced = signed_channel_tx(&existing, U256::from(1), 0, 1_001);
        assert!(pool.insert_validated(valid_pool_transaction(underpriced), 0).is_err());

        assert_eq!(pool.usage.sender_ids.len(), 1);
    }

    fn signed_1559(signer: &PrivateKeySigner, nonce: u64, max_fee: u128) -> BasePooledTransaction {
        let tx = TxEip1559 {
            chain_id: test_chain_id(),
            nonce,
            gas_limit: 50_000,
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(Address::repeat_byte(0xEE)),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered = envelope.clone().try_into_recovered().unwrap();
        BasePooledTransaction::new(recovered, envelope.encode_2718_len())
    }

    fn protocol_lane_pool() -> TwoDNoncePool<BasePooledTransaction> {
        TwoDNoncePool::new(PriceBumpConfig::default()).with_protocol_nonce_lanes()
    }

    #[test]
    fn protocol_nonce_transactions_need_protocol_lanes() {
        let signer = signer();
        let mut channel_only = TwoDNoncePool::new(PriceBumpConfig::default());
        let rejected = valid_pool_transaction(signed_1559(&signer, 0, 1_000));
        let rejected_hash = *rejected.hash();
        let error = channel_only.insert_validated(rejected, 0).unwrap_err();
        assert_eq!(error.hash, rejected_hash);
        assert_eq!(channel_only.len(), 0);

        // EIP-1559 and key-0 EIP-8130 transactions share the sender's lane 0.
        let mut pool = protocol_lane_pool();
        let eip1559 = valid_pool_transaction(signed_1559(&signer, 0, 1_000));
        let key0 = valid_pool_transaction(signed_channel_tx(&signer, U256::ZERO, 1, 1_000));
        let (eip1559_hash, key0_hash) = (*eip1559.hash(), *key0.hash());
        pool.insert_validated(eip1559, 0).unwrap();
        pool.insert_validated(key0, 0).unwrap();
        let lane = (signer.address(), U256::ZERO);
        assert_eq!(pool.lane_cursor(&lane), Some(LaneCursor::at(0)));
        assert_eq!(hashes(&pool.pending_transactions()), vec![eip1559_hash, key0_hash]);
        assert_eq!(pool.lane_pending_end(&lane), Some(2));
        assert_eq!(
            pool.highest_consecutive_from(&lane, 0).map(|transaction| *transaction.hash()),
            Some(key0_hash)
        );
        assert_eq!(pool.len(), 2);
        assert!(pool.size() > 0);
    }

    #[test]
    fn a_cursor_ahead_of_state_makes_the_run_behind_it_executable() {
        let signer = signer();
        let lane = (signer.address(), U256::ZERO);
        let mut pool = protocol_lane_pool();
        let cursor = LaneCursor { state_nonce: 0, next_nonce: 2 };

        let behind = valid_pool_transaction(signed_1559(&signer, 1, 1_000));
        let error = pool.insert_validated_at(behind, cursor).unwrap_err();
        assert!(
            matches!(
                error.kind,
                PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::Consensus(
                    InvalidTransactionError::NonceNotConsistent { tx: 1, state: 2 }
                ))
            ),
            "{error:?}"
        );
        assert!(pool.lane_cursor(&lane).is_none(), "a rejected insert creates no lane");

        let head = valid_pool_transaction(signed_1559(&signer, 2, 1_000));
        let head_hash = *head.hash();
        let outcome = pool.insert_validated_at(head, cursor).unwrap();
        assert!(matches!(outcome.outcome.state, AddedTransactionState::Pending));
        assert_eq!(pool.lane_cursor(&lane), Some(cursor));
        assert_eq!(*pool.lanes_ahead_of_state(), HashMap::from_iter([(lane, 0)]));
        assert_eq!(hashes(&pool.pending_transactions()), vec![head_hash]);
        assert!(pool.lane_pending_transaction(&lane, 2).is_some());
        assert!(pool.highest_consecutive_from(&lane, 1).is_none(), "below the cursor");
        assert_eq!(
            pool.highest_consecutive_from(&lane, 2).map(|transaction| *transaction.hash()),
            Some(head_hash)
        );
    }

    #[test]
    fn set_lane_cursor_moves_the_executable_cursor_in_both_directions() {
        let signer = signer();
        let lane = (signer.address(), U256::ZERO);
        let mut pool = protocol_lane_pool();
        let ahead = LaneCursor { state_nonce: 0, next_nonce: 2 };
        let [second, third] =
            [2, 3].map(|nonce| valid_pool_transaction(signed_1559(&signer, nonce, 1_000)));
        let (second_hash, third_hash) = (*second.hash(), *third.hash());
        pool.insert_validated_at(second, ahead).unwrap();
        pool.insert_validated_at(third, ahead).unwrap();

        // The run in front was mined up to the cursor: the lane no longer depends on it.
        pool.set_lane_cursor(lane, LaneCursor::at(2));
        assert!(pool.lanes_ahead_of_state().is_empty());
        pool.set_lane_cursor(lane, ahead);
        assert_eq!(*pool.lanes_ahead_of_state(), HashMap::from_iter([(lane, 0)]));

        // The standard run in front disappeared: nothing is executable, nothing is lost.
        let update = pool.set_lane_cursor(lane, LaneCursor::at(0));
        assert_eq!(hashes(&update.queued), vec![second_hash, third_hash]);
        assert!(update.discarded.is_empty() && update.promoted.is_empty());
        assert!(pool.pending_transactions().is_empty());
        assert!(pool.lanes_ahead_of_state().is_empty());
        assert_eq!(pool.lane_cursor(&lane), Some(LaneCursor::at(0)));

        // The run in front now reaches nonce 3: nonce 2 can never execute here.
        let update = pool.set_lane_cursor(lane, LaneCursor { state_nonce: 0, next_nonce: 3 });
        assert_eq!(hashes(&update.discarded), vec![second_hash]);
        assert_eq!(hashes(&update.promoted), vec![third_hash]);
        assert!(!pool.contains(&second_hash));

        // Canonical maintenance only moves the anchor forward and never below the cursor.
        assert!(pool.advance_lane_nonce(lane, 2).discarded.is_empty());
        assert_eq!(pool.lane_cursor(&lane), Some(LaneCursor { state_nonce: 2, next_nonce: 3 }));
        assert!(pool.advance_lane_nonce(lane, 1).discarded.is_empty());
        assert_eq!(pool.lane_cursor(&lane), Some(LaneCursor { state_nonce: 2, next_nonce: 3 }));

        // Past the lane's last transaction the lane is dropped entirely.
        let update = pool.advance_lane_nonce(lane, 4);
        assert_eq!(hashes(&update.discarded), vec![third_hash]);
        assert!(pool.lane_cursor(&lane).is_none());
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn nonce_free_transactions_are_found_by_replay_id() {
        let signer = signer();
        let mut pool = protocol_lane_pool();
        let transaction = valid_pool_transaction(signed_nonce_free_tx(&signer, 10_000, 0, 1_000));
        let replay_id = transaction.transaction.eip8130_replay_id().unwrap();
        let hash = *transaction.hash();
        pool.insert_validated(transaction, 0).unwrap();

        assert_eq!(pool.nonce_free_transaction(&replay_id).map(|tx| *tx.hash()), Some(hash));
        assert!(pool.lanes_ahead_of_state().is_empty(), "nonce-free transactions have no lane");
        pool.remove_transactions(&[hash]);
        assert!(pool.nonce_free_transaction(&replay_id).is_none());
        assert_eq!((pool.len(), pool.size()), (0, 0));
    }
}
