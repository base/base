//! 2D-nonce-aware transaction pool for EIP-8130 AA transactions.
//!
//! Reth's standard pool uses `(sender, nonce_sequence)` as the identity key
//! and tracks account nonces from EVM state. EIP-8130 transactions use the
//! `NONCE_MANAGER` contract for nonce management (even at `nonce_key == 0`),
//! need expiry tracking, and can have multiple lanes that collide in the
//! standard pool.
//!
//! This module provides an [`Eip8130Pool`] that stores **all** EIP-8130
//! transactions, keyed by the full 2D identity `(sender, nonce_key,
//! nonce_sequence)`. The standard Reth pool still receives them for RPC
//! backward compatibility, but does not drive nonce ordering, expiry, or
//! P2P gossip for these transactions.
//!
//! Modeled on Tempo's `AA2dPool` architecture.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, U256};
use base_alloy_consensus::lock_slot;
use parking_lot::RwLock;
use reth_transaction_pool::{
    EthPoolTransaction, PoolTransaction, TransactionOrigin, ValidPoolTransaction,
    error::InvalidPoolTransactionError,
    identifier::{SenderId, TransactionId},
};
use tokio::sync::broadcast;

/// Identifies a nonce sequence lane: `(sender, nonce_key)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Eip8130SequenceId {
    /// Transaction sender.
    pub sender: Address,
    /// Nonce key (non-zero for 2D nonce lanes).
    pub nonce_key: U256,
}

/// Full 2D identity for an AA transaction.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Eip8130TxId {
    /// Transaction sender.
    pub sender: Address,
    /// Nonce key.
    pub nonce_key: U256,
    /// Nonce sequence within the key.
    pub nonce_sequence: u64,
}

impl Eip8130TxId {
    /// Returns the sequence lane this tx belongs to.
    pub fn sequence_id(&self) -> Eip8130SequenceId {
        Eip8130SequenceId { sender: self.sender, nonce_key: self.nonce_key }
    }
}

/// An entry stored in the pool alongside the full transaction.
///
/// No trait bounds on `T` at the struct level — bounds are only required on
/// impl blocks that interact with `ValidPoolTransaction` or `PoolTransaction`.
struct PooledEntry<T> {
    id: Eip8130TxId,
    transaction: T,
    origin: TransactionOrigin,
    timestamp: Instant,
    /// Whether this transaction is executable (contiguous nonce chain from on-chain nonce).
    is_pending: bool,
    /// Monotonic insertion counter for eviction tie-breaking.
    submission_id: u64,
    /// Cached `max_priority_fee_per_gas` for eviction ordering.
    priority_fee: u128,
    /// Unix timestamp after which this transaction is invalid. `0` = no expiry.
    expiry: u64,
}

impl<T: core::fmt::Debug> core::fmt::Debug for PooledEntry<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PooledEntry")
            .field("id", &self.id)
            .field("transaction", &self.transaction)
            .field("origin", &self.origin)
            .field("timestamp", &self.timestamp)
            .field("is_pending", &self.is_pending)
            .finish()
    }
}

/// Key for ordering transactions in the eviction set.
///
/// Lower priority is evicted first. On equal priority, newer submissions
/// (higher `submission_id`) are evicted first to favour older transactions.
/// The hash field provides a unique tiebreaker for `BTreeSet`.
#[derive(Debug, Clone)]
struct EvictionKey {
    priority_fee: u128,
    submission_id: u64,
    hash: B256,
}

impl PartialEq for EvictionKey {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}

impl Eq for EvictionKey {}

impl PartialOrd for EvictionKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EvictionKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority_fee
            .cmp(&other.priority_fee)
            .then_with(|| other.submission_id.cmp(&self.submission_id))
            .then_with(|| self.hash.cmp(&other.hash))
    }
}

/// Result of a successful [`Eip8130Pool::add_transaction`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    /// Transaction was inserted as a new entry.
    Added,
    /// Transaction replaced an existing entry at the same 2D nonce.
    Replaced,
}

/// Per-sequence state: on-chain nonce and ordered map of pool transactions.
///
/// Individual entries track their own `is_pending` status. Transactions with
/// `nonce_sequence` forming a contiguous chain from `next_nonce` are pending;
/// those after a gap are queued.
struct SequenceState<T> {
    next_nonce: u64,
    txs: BTreeMap<u64, PooledEntry<T>>,
}

impl<T: core::fmt::Debug> core::fmt::Debug for SequenceState<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SequenceState")
            .field("next_nonce", &self.next_nonce)
            .field("txs", &self.txs)
            .finish()
    }
}

impl<T> Default for SequenceState<T> {
    fn default() -> Self {
        Self { next_nonce: 0, txs: BTreeMap::new() }
    }
}

/// Throughput tier for an account, determined lazily by the pool when an
/// account is about to breach the default cap.
///
/// The tier controls separate limits for the sender and payer roles:
/// - [`Default`](Self::Default): base limits for both roles.
/// - [`Locked`](Self::Locked): elevated **sender** limit (account code is
///   immutable).
/// - [`LockedTrustedBytecode`](Self::LockedTrustedBytecode): elevated limits
///   for **both** sender and payer (locked + bytecode in the trusted set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ThroughputTier {
    /// Account has not been checked or does not qualify — default throughput.
    #[default]
    Default,
    /// Account is locked — elevated sender throughput.
    Locked,
    /// Account is locked and has trusted bytecode — elevated sender and payer
    /// throughput.
    LockedTrustedBytecode,
}

/// Result of a lazy tier check, returned by the `check_tier` closure.
#[derive(Debug, Clone, Copy)]
pub struct TierCheckResult {
    /// The resolved throughput tier for the account.
    pub tier: ThroughputTier,
    /// Suggested cache lifetime based on the on-chain unlock deadline. `None`
    /// means the account is not locked and the pool should use its default
    /// TTL. When `Some`, the pool caches for `min(duration, config.tier_cache_ttl)`.
    pub cache_for: Option<Duration>,
}

/// Configuration for the EIP-8130 2D nonce pool.
#[derive(Debug, Clone)]
pub struct Eip8130PoolConfig {
    /// Maximum transactions per sequence lane `(sender, nonce_key)`.
    pub max_txs_per_sequence: usize,
    /// Maximum total transactions in the pool.
    pub max_pool_size: usize,
    /// Sender-role limit for accounts at the default tier.
    pub default_max_sender_txs: usize,
    /// Sender-role limit for locked accounts.
    pub locked_max_sender_txs: usize,
    /// Sender-role limit for locked accounts with trusted bytecode.
    pub trusted_max_sender_txs: usize,
    /// Payer-role limit for accounts at the default (and locked) tier.
    pub default_max_payer_txs: usize,
    /// Payer-role limit for locked accounts with trusted bytecode.
    pub trusted_max_payer_txs: usize,
    /// Maximum time a cached tier remains valid. The actual expiry may be
    /// shorter when the on-chain unlock deadline is sooner.
    pub tier_cache_ttl: Duration,
    /// Minimum percentage fee increase required for replacement transactions.
    /// A value of 10 means a 10% bump is required (e.g. 100 -> 110).
    pub price_bump_percent: u64,
}

impl Default for Eip8130PoolConfig {
    fn default() -> Self {
        Self {
            max_txs_per_sequence: 16,
            max_pool_size: 4096,
            default_max_sender_txs: 8,
            locked_max_sender_txs: 64,
            trusted_max_sender_txs: 128,
            default_max_payer_txs: 8,
            trusted_max_payer_txs: 128,
            tier_cache_ttl: Duration::from_secs(300),
            price_bump_percent: 10,
        }
    }
}

impl Eip8130PoolConfig {
    /// Returns the sender-role transaction cap for the given tier.
    pub fn max_sender_txs_for_tier(&self, tier: ThroughputTier) -> usize {
        match tier {
            ThroughputTier::Default => self.default_max_sender_txs,
            ThroughputTier::Locked => self.locked_max_sender_txs,
            ThroughputTier::LockedTrustedBytecode => self.trusted_max_sender_txs,
        }
    }

    /// Returns the payer-role transaction cap for the given tier.
    pub fn max_payer_txs_for_tier(&self, tier: ThroughputTier) -> usize {
        match tier {
            ThroughputTier::Default | ThroughputTier::Locked => self.default_max_payer_txs,
            ThroughputTier::LockedTrustedBytecode => self.trusted_max_payer_txs,
        }
    }
}

/// Cached throughput tier with a monotonic expiry.
#[derive(Debug, Clone, Copy)]
struct CachedTier {
    tier: ThroughputTier,
    expires_at: Instant,
}

struct PoolInner<T> {
    sequences: HashMap<Eip8130SequenceId, SequenceState<T>>,
    by_hash: HashMap<B256, Eip8130TxId>,
    /// Reverse index: nonce storage slot → sequence ID.
    slot_to_seq: HashMap<B256, Eip8130SequenceId>,
    /// Per-account count of pool txs where the account acts as **sender**.
    sender_txs: HashMap<Address, usize>,
    /// Per-account count of pool txs where the account acts as **payer**.
    payer_txs: HashMap<Address, usize>,
    /// Payer address for each tx hash.
    payer_by_hash: HashMap<B256, Address>,
    /// Cached throughput tier per account.
    account_tiers: HashMap<Address, CachedTier>,
    /// Reverse map: lock storage slot → account address.
    lock_slot_to_account: HashMap<B256, Address>,
    /// Monotonic counter for eviction tie-breaking.
    next_submission_id: u64,
    /// All transactions ordered by eviction priority (lowest first).
    by_eviction_order: BTreeSet<EvictionKey>,
}

impl<T: core::fmt::Debug> core::fmt::Debug for PoolInner<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PoolInner")
            .field("sequences", &self.sequences)
            .field("by_hash_len", &self.by_hash.len())
            .field("slot_to_seq_len", &self.slot_to_seq.len())
            .field("sender_txs_len", &self.sender_txs.len())
            .field("payer_txs_len", &self.payer_txs.len())
            .field("eviction_set_len", &self.by_eviction_order.len())
            .finish()
    }
}

impl<T> Default for PoolInner<T> {
    fn default() -> Self {
        Self {
            sequences: HashMap::new(),
            by_hash: HashMap::new(),
            slot_to_seq: HashMap::new(),
            sender_txs: HashMap::new(),
            payer_txs: HashMap::new(),
            payer_by_hash: HashMap::new(),
            account_tiers: HashMap::new(),
            lock_slot_to_account: HashMap::new(),
            next_submission_id: 0,
            by_eviction_order: BTreeSet::new(),
        }
    }
}

/// A 2D-nonce-aware pool for EIP-8130 transactions with `nonce_key != 0`.
///
/// Thread-safe via interior `RwLock`. All public methods acquire the lock
/// internally, so callers do not need external synchronization.
///
/// The type parameter `T` is the pool transaction type (e.g.
/// [`BasePooledTransaction`](crate::BasePooledTransaction)). No trait bounds
/// are required on the struct itself, only on the methods that need them.
pub struct Eip8130Pool<T> {
    inner: RwLock<PoolInner<T>>,
    config: Eip8130PoolConfig,
    /// Broadcasts the hash of every newly-pending EIP-8130 transaction so
    /// that [`BaseTransactionPool`](crate::BaseTransactionPool) can merge it
    /// into the P2P gossip stream.
    pending_tx_sender: broadcast::Sender<B256>,
}

/// Capacity of the broadcast channel for pending EIP-8130 transaction hashes.
const PENDING_TX_BROADCAST_CAPACITY: usize = 256;

impl<T: core::fmt::Debug> core::fmt::Debug for Eip8130Pool<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Eip8130Pool")
            .field("config", &self.config)
            .field("inner", &*self.inner.read())
            .finish()
    }
}

impl<T> Default for Eip8130Pool<T> {
    fn default() -> Self {
        Self::with_config(Eip8130PoolConfig::default())
    }
}

impl<T> Eip8130Pool<T> {
    /// Creates an empty pool.
    pub fn new() -> Self {
        Self::with_config(Eip8130PoolConfig::default())
    }

    /// Creates a pool with the given configuration.
    pub fn with_config(config: Eip8130PoolConfig) -> Self {
        let (pending_tx_sender, _) = broadcast::channel(PENDING_TX_BROADCAST_CAPACITY);
        Self { inner: RwLock::new(PoolInner::default()), config, pending_tx_sender }
    }

    /// Returns a reference to the pool configuration.
    pub fn config(&self) -> &Eip8130PoolConfig {
        &self.config
    }

    /// Subscribes to newly-pending EIP-8130 transaction hashes.
    ///
    /// Each time `add_transaction` marks a transaction as pending, its hash is
    /// broadcast to all active receivers. Used by [`BaseTransactionPool`](crate::BaseTransactionPool)
    /// to merge EIP-8130 events into the P2P gossip listener.
    pub fn subscribe_pending_transactions(&self) -> broadcast::Receiver<B256> {
        self.pending_tx_sender.subscribe()
    }

    /// Returns `true` if the pool contains a transaction with the given hash.
    pub fn contains(&self, hash: &B256) -> bool {
        self.inner.read().by_hash.contains_key(hash)
    }

    /// Returns the 2D identity for a transaction hash, if present.
    pub fn get_id(&self, hash: &B256) -> Option<Eip8130TxId> {
        self.inner.read().by_hash.get(hash).cloned()
    }

    /// Number of transactions currently in the pool.
    pub fn len(&self) -> usize {
        self.inner.read().by_hash.len()
    }

    /// Returns `true` if the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.read().by_hash.is_empty()
    }

    /// Looks up the sequence ID for a nonce storage slot, if tracked.
    pub fn seq_id_for_slot(&self, slot: &B256) -> Option<Eip8130SequenceId> {
        self.inner.read().slot_to_seq.get(slot).cloned()
    }

    /// Resets the cached throughput tier for an account, forcing re-evaluation
    /// on the next insertion that would breach the default cap.
    ///
    /// Called by the invalidation task when the account's lock slot changes
    /// on-chain (e.g. the account was unlocked).
    pub fn invalidate_account_tier(&self, account: &Address) {
        self.inner.write().account_tiers.remove(account);
    }

    /// Checks a set of changed `ACCOUNT_CONFIG_ADDRESS` storage slots against
    /// the pool's lock-slot reverse map and invalidates the cached throughput
    /// tier for any matching accounts.
    ///
    /// Returns the number of accounts whose tiers were invalidated.
    pub fn invalidate_tiers_for_lock_slots(&self, changed_slots: &[B256]) -> usize {
        let mut inner = self.inner.write();
        let mut invalidated = 0;
        for slot in changed_slots {
            if let Some(account) = inner.lock_slot_to_account.get(slot).copied() {
                if inner.account_tiers.remove(&account).is_some() {
                    invalidated += 1;
                }
            }
        }
        invalidated
    }

    /// Returns all transaction hashes in the pool.
    pub fn all_hashes(&self) -> Vec<B256> {
        self.inner.read().by_hash.keys().copied().collect()
    }

    /// Removes a transaction by hash. Returns the id if found.
    pub fn remove_transaction(&self, hash: &B256) -> Option<Eip8130TxId> {
        let mut inner = self.inner.write();
        Self::remove_from_inner(&mut inner, hash)
    }

    /// Removes multiple transactions by hash. Returns the hashes that were
    /// actually present.
    pub fn remove_transactions(&self, hashes: &[B256]) -> Vec<B256> {
        let mut inner = self.inner.write();
        hashes.iter().filter_map(|h| Self::remove_from_inner(&mut inner, h).map(|_| *h)).collect()
    }

    /// Removes all transactions whose `expiry` is non-zero and less than or
    /// equal to `current_timestamp`. Returns the hashes of removed transactions.
    pub fn sweep_expired(&self, current_timestamp: u64) -> Vec<B256>
    where
        T: PoolTransaction,
    {
        let inner_read = self.inner.read();
        let expired_hashes: Vec<B256> = inner_read
            .by_hash
            .keys()
            .filter_map(|hash| {
                let id = inner_read.by_hash.get(hash)?;
                let seq_id = Eip8130SequenceId { sender: id.sender, nonce_key: id.nonce_key };
                let seq = inner_read.sequences.get(&seq_id)?;
                let entry = seq.txs.get(&id.nonce_sequence)?;
                if entry.expiry != 0 && entry.expiry <= current_timestamp {
                    Some(*hash)
                } else {
                    None
                }
            })
            .collect();
        drop(inner_read);

        if expired_hashes.is_empty() {
            return Vec::new();
        }
        self.remove_transactions(&expired_hashes)
    }

    /// Updates the known on-chain nonce for a sequence lane and removes any
    /// transactions with `nonce_sequence < new_nonce`. Remaining transactions
    /// are re-evaluated for pending/queued status via promotion scan.
    ///
    /// Returns the hashes of pruned transactions.
    pub fn update_sequence_nonce(&self, seq_id: &Eip8130SequenceId, new_nonce: u64) -> Vec<B256>
    where
        T: PoolTransaction,
    {
        let mut inner = self.inner.write();
        let mut removed_hashes = Vec::new();
        let mut eviction_keys_to_remove = Vec::new();

        if let Some(seq) = inner.sequences.get_mut(seq_id) {
            seq.next_nonce = new_nonce;
            let stale: Vec<u64> = seq.txs.range(..new_nonce).map(|(&nonce, _)| nonce).collect();
            for nonce in stale {
                if let Some(entry) = seq.txs.remove(&nonce) {
                    let hash = *entry.transaction.hash();
                    eviction_keys_to_remove.push(EvictionKey {
                        priority_fee: entry.priority_fee,
                        submission_id: entry.submission_id,
                        hash,
                    });
                    removed_hashes.push(hash);
                }
            }
            Self::promote_sequence(seq);
        }

        for key in eviction_keys_to_remove {
            inner.by_eviction_order.remove(&key);
        }

        let sender = seq_id.sender;
        let mut sponsored_count = 0usize;
        for hash in &removed_hashes {
            inner.by_hash.remove(hash);
            if let Some(payer) = inner.payer_by_hash.remove(hash) {
                Self::decrement_counter(&mut inner.payer_txs, &payer, 1);
                if payer != sender {
                    sponsored_count += 1;
                    Self::maybe_clear_tier(&mut inner, &payer);
                }
            }
        }

        if sponsored_count > 0 {
            Self::decrement_counter(&mut inner.sender_txs, &sender, sponsored_count);
        }
        if !removed_hashes.is_empty() {
            Self::maybe_clear_tier(&mut inner, &sender);
        }

        if inner.sequences.get(seq_id).is_some_and(|s| s.txs.is_empty()) {
            inner.sequences.remove(seq_id);
            inner.slot_to_seq.retain(|_, v| v != seq_id);
        }

        removed_hashes
    }

    fn remove_from_inner(inner: &mut PoolInner<T>, hash: &B256) -> Option<Eip8130TxId> {
        let id = inner.by_hash.remove(hash)?;
        let seq_id = id.sequence_id();
        let entry = inner.sequences.get_mut(&seq_id)?.txs.remove(&id.nonce_sequence)?;

        inner.by_eviction_order.remove(&EvictionKey {
            priority_fee: entry.priority_fee,
            submission_id: entry.submission_id,
            hash: *hash,
        });

        // Demote descendants — removing this tx creates a nonce gap.
        if let Some(seq) = inner.sequences.get_mut(&seq_id) {
            Self::demote_from_nonce(seq, id.nonce_sequence);
        }

        let sender = id.sender;
        if let Some(payer) = inner.payer_by_hash.remove(hash) {
            Self::decrement_counter(&mut inner.payer_txs, &payer, 1);
            if payer != sender {
                Self::decrement_counter(&mut inner.sender_txs, &sender, 1);
                Self::maybe_clear_tier(inner, &payer);
            }
        } else {
            Self::decrement_counter(&mut inner.sender_txs, &sender, 1);
        }
        Self::maybe_clear_tier(inner, &sender);

        if inner.sequences.get(&seq_id).is_some_and(|s| s.txs.is_empty()) {
            inner.sequences.remove(&seq_id);
            inner.slot_to_seq.retain(|_, v| v != &seq_id);
        }
        Some(id)
    }

    /// Decrements a counter map entry, removing it when it reaches zero.
    fn decrement_counter(map: &mut HashMap<Address, usize>, account: &Address, n: usize) {
        use std::collections::hash_map::Entry;
        if let Entry::Occupied(mut entry) = map.entry(*account) {
            let count = entry.get_mut();
            *count = count.saturating_sub(n);
            if *count == 0 {
                entry.remove();
            }
        }
    }

    /// Removes the cached tier and lock-slot reverse entry when an account
    /// has no remaining sender or payer transactions.
    fn maybe_clear_tier(inner: &mut PoolInner<T>, account: &Address) {
        let has_sender = inner.sender_txs.contains_key(account);
        let has_payer = inner.payer_txs.contains_key(account);
        if !has_sender && !has_payer {
            inner.account_tiers.remove(account);
            inner.lock_slot_to_account.remove(&lock_slot(*account));
        }
    }

    /// Evicts the single lowest-priority transaction from the pool.
    ///
    /// Prefers queued transactions. Returns `true` if something was evicted.
    fn try_evict_one(inner: &mut PoolInner<T>, _config: &Eip8130PoolConfig) -> bool
    where
        T: PoolTransaction,
    {
        // First pass: find the lowest-priority queued tx.
        let queued_hash = inner
            .by_eviction_order
            .iter()
            .find(|key| {
                // Check if the tx behind this key is queued (not pending).
                if let Some(id) = inner.by_hash.get(&key.hash) {
                    let seq_id = id.sequence_id();
                    inner
                        .sequences
                        .get(&seq_id)
                        .and_then(|s| s.txs.get(&id.nonce_sequence))
                        .is_some_and(|e| !e.is_pending)
                } else {
                    false
                }
            })
            .map(|key| key.hash);

        // Fall back to lowest-priority pending tx if no queued txs exist.
        let hash_to_evict =
            queued_hash.or_else(|| inner.by_eviction_order.iter().next().map(|key| key.hash));

        if let Some(hash) = hash_to_evict {
            Self::remove_from_inner(inner, &hash);
            true
        } else {
            false
        }
    }

    /// Walks the sequence forward from `next_nonce` and sets `is_pending`
    /// for every entry in a contiguous run, then `false` for anything
    /// after the first gap.
    fn promote_sequence(seq: &mut SequenceState<T>) {
        let mut next = seq.next_nonce;
        let mut hit_gap = false;
        for (&nonce, entry) in seq.txs.iter_mut() {
            if !hit_gap && nonce == next {
                entry.is_pending = true;
                next += 1;
            } else {
                hit_gap = true;
                entry.is_pending = false;
            }
        }
    }

    /// Marks all entries with `nonce_sequence > removed_nonce` as queued.
    fn demote_from_nonce(seq: &mut SequenceState<T>, removed_nonce: u64) {
        for (_, entry) in seq
            .txs
            .range_mut((std::ops::Bound::Excluded(removed_nonce), std::ops::Bound::Unbounded))
        {
            entry.is_pending = false;
        }
    }

    /// Returns `true` if `new_fee` satisfies the price bump over `old_fee`.
    fn meets_price_bump(old_fee: u128, new_fee: u128, bump_pct: u64) -> bool {
        let min_new = old_fee.saturating_mul(100 + bump_pct as u128) / 100;
        new_fee >= min_new
    }
}

impl<T: PoolTransaction> Eip8130Pool<T> {
    /// Attempts to add a validated transaction to the pool.
    ///
    /// If a transaction already exists at the same `(sender, nonce_key,
    /// nonce_sequence)`, the new transaction replaces it **only** if its
    /// `max_priority_fee_per_gas` meets the configured price bump. Otherwise
    /// a [`Eip8130PoolError::ReplacementUnderpriced`] error is returned.
    ///
    /// On success the pool runs a promotion scan: any previously queued
    /// transactions that now form a contiguous nonce chain are marked pending.
    /// If the pool exceeds `max_pool_size`, the lowest-priority queued (then
    /// pending) transactions are evicted.
    ///
    /// **Counting rules:** a self-pay transaction (payer == sender) increments
    /// the payer counter only. A sponsored transaction increments the sender's
    /// sender counter and the payer's payer counter.
    pub fn add_transaction(
        &self,
        id: Eip8130TxId,
        transaction: T,
        payer: Address,
        origin: TransactionOrigin,
        nonce_storage_slot: B256,
        expiry: u64,
        check_tier: &dyn Fn(Address) -> TierCheckResult,
    ) -> Result<AddOutcome, Eip8130PoolError> {
        let hash = *transaction.hash();
        let priority_fee = transaction.max_priority_fee_per_gas().unwrap_or_default();
        let mut inner = self.inner.write();

        if inner.by_hash.contains_key(&hash) {
            return Err(Eip8130PoolError::DuplicateHash(hash));
        }

        let sender = id.sender;
        let is_self_pay = payer == sender;
        let seq_id = id.sequence_id();

        // --- Replacement check ---
        let replaced_hash: Option<B256> = {
            let seq = inner.sequences.entry(seq_id.clone()).or_default();
            if let Some(existing) = seq.txs.get(&id.nonce_sequence) {
                if !Self::meets_price_bump(
                    existing.priority_fee,
                    priority_fee,
                    self.config.price_bump_percent,
                ) {
                    return Err(Eip8130PoolError::ReplacementUnderpriced {
                        existing_fee: existing.priority_fee,
                        new_fee: priority_fee,
                    });
                }
                Some(*existing.transaction.hash())
            } else {
                None
            }
        };

        // Remove the replaced tx (if any) before capacity checks.
        if let Some(old_hash) = replaced_hash {
            Self::remove_from_inner(&mut inner, &old_hash);
        }

        // --- Capacity / account limit checks (only for new insertions, not replacements) ---
        if replaced_hash.is_none() {
            if inner.by_hash.len() >= self.config.max_pool_size {
                // Try evicting before rejecting.
                if !Self::try_evict_one(&mut inner, &self.config) {
                    return Err(Eip8130PoolError::PoolFull);
                }
            }

            if !is_self_pay {
                let sender_count = inner.sender_txs.get(&sender).copied().unwrap_or(0);
                if sender_count >= self.config.default_max_sender_txs {
                    let tier = self.resolve_tier(&mut inner, sender, check_tier);
                    if sender_count >= self.config.max_sender_txs_for_tier(tier) {
                        return Err(Eip8130PoolError::AccountCapacityExceeded(sender));
                    }
                }
            }

            let payer_count = inner.payer_txs.get(&payer).copied().unwrap_or(0);
            if payer_count >= self.config.default_max_payer_txs {
                let tier = self.resolve_tier(&mut inner, payer, check_tier);
                if payer_count >= self.config.max_payer_txs_for_tier(tier) {
                    return Err(Eip8130PoolError::AccountCapacityExceeded(payer));
                }
            }

            let seq = inner.sequences.entry(seq_id.clone()).or_default();
            if seq.txs.len() >= self.config.max_txs_per_sequence {
                return Err(Eip8130PoolError::SequenceFull);
            }
        }

        // --- Insert ---
        let sub_id = inner.next_submission_id;
        inner.next_submission_id += 1;

        let entry = PooledEntry {
            id: id.clone(),
            transaction,
            origin,
            timestamp: Instant::now(),
            is_pending: false,
            submission_id: sub_id,
            priority_fee,
            expiry,
        };

        // Ensure the sequence exists (it may have been cleaned up by
        // remove_from_inner during replacement if it was the only entry).
        let seq = inner.sequences.entry(seq_id.clone()).or_default();
        seq.txs.insert(id.nonce_sequence, entry);

        // Promotion scan — sets is_pending for the contiguous chain.
        // Collect hashes of transactions that become newly pending.
        let pending_before: HashSet<u64> = seq
            .txs
            .iter()
            .filter_map(|(n, e)| if e.is_pending { Some(*n) } else { None })
            .collect();
        Self::promote_sequence(seq);
        let newly_pending: Vec<B256> = seq
            .txs
            .iter()
            .filter(|(n, e)| e.is_pending && !pending_before.contains(n))
            .map(|(_, e)| *e.transaction.hash())
            .collect();

        inner.by_hash.insert(hash, id);
        inner.payer_by_hash.insert(hash, payer);
        inner.slot_to_seq.entry(nonce_storage_slot).or_insert(seq_id);
        inner.by_eviction_order.insert(EvictionKey { priority_fee, submission_id: sub_id, hash });

        if replaced_hash.is_none() {
            *inner.payer_txs.entry(payer).or_insert(0) += 1;
            if !is_self_pay {
                *inner.sender_txs.entry(sender).or_insert(0) += 1;
            }
        }

        // Notify P2P listeners about newly pending transactions.
        for pending_hash in &newly_pending {
            let _ = self.pending_tx_sender.send(*pending_hash);
        }

        Ok(if replaced_hash.is_some() { AddOutcome::Replaced } else { AddOutcome::Added })
    }

    /// Resolves the throughput tier for `account`, using the cache when fresh
    /// and falling back to `check_tier` otherwise.
    fn resolve_tier(
        &self,
        inner: &mut PoolInner<T>,
        account: Address,
        check_tier: &dyn Fn(Address) -> TierCheckResult,
    ) -> ThroughputTier {
        let now = Instant::now();
        if let Some(cached) = inner.account_tiers.get(&account) {
            if now < cached.expires_at {
                return cached.tier;
            }
        }
        let result = check_tier(account);
        let ttl = result
            .cache_for
            .map_or(self.config.tier_cache_ttl, |d| d.min(self.config.tier_cache_ttl));
        inner
            .account_tiers
            .insert(account, CachedTier { tier: result.tier, expires_at: now + ttl });
        inner.lock_slot_to_account.entry(lock_slot(account)).or_insert(account);
        result.tier
    }

    /// Returns the validated pool transaction for the given hash, if present.
    pub fn get(&self, hash: &B256) -> Option<Arc<ValidPoolTransaction<T>>>
    where
        T: Clone,
    {
        let inner = self.inner.read();
        let id = inner.by_hash.get(hash)?;
        let seq_id = id.sequence_id();
        let entry = inner.sequences.get(&seq_id)?.txs.get(&id.nonce_sequence)?;
        Some(Self::wrap_entry(entry))
    }

    /// Returns `(pending, queued)` transaction counts using the tracked
    /// `is_pending` flag on each entry.
    pub fn pending_and_queued_count(&self) -> (usize, usize) {
        let inner = self.inner.read();
        let mut pending = 0;
        let mut queued = 0;
        for seq in inner.sequences.values() {
            for entry in seq.txs.values() {
                if entry.is_pending {
                    pending += 1;
                } else {
                    queued += 1;
                }
            }
        }
        (pending, queued)
    }

    /// Returns how many pool transactions list `account` as the sender
    /// (excludes self-pay, which counts under payer only).
    pub fn sender_tx_count(&self, account: &Address) -> usize {
        self.inner.read().sender_txs.get(account).copied().unwrap_or(0)
    }

    /// Returns how many pool transactions list `account` as the payer
    /// (includes self-pay).
    pub fn payer_tx_count(&self, account: &Address) -> usize {
        self.inner.read().payer_txs.get(account).copied().unwrap_or(0)
    }

    /// Returns all transactions from a specific sender across all nonce lanes.
    pub fn get_transactions_by_sender(&self, sender: &Address) -> Vec<Arc<ValidPoolTransaction<T>>>
    where
        T: Clone,
    {
        let inner = self.inner.read();
        inner
            .sequences
            .iter()
            .filter(|(seq_id, _)| &seq_id.sender == sender)
            .flat_map(|(_, state)| state.txs.values().map(|e| Self::wrap_entry(e)))
            .collect()
    }

    /// Returns all pending (ready) transactions.
    pub fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>>
    where
        T: Clone,
    {
        let inner = self.inner.read();
        inner
            .sequences
            .values()
            .flat_map(|seq| seq.txs.values().filter(|e| e.is_pending).map(|e| Self::wrap_entry(e)))
            .collect()
    }

    /// Returns all queued (not yet ready) transactions.
    pub fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>>
    where
        T: Clone,
    {
        let inner = self.inner.read();
        inner
            .sequences
            .values()
            .flat_map(|seq| seq.txs.values().filter(|e| !e.is_pending).map(|e| Self::wrap_entry(e)))
            .collect()
    }

    /// Returns all validated transactions in the pool (regardless of readiness).
    pub fn all_transactions(&self) -> Vec<Arc<ValidPoolTransaction<T>>>
    where
        T: Clone,
    {
        let inner = self.inner.read();
        inner
            .sequences
            .values()
            .flat_map(|seq| seq.txs.values().map(|e| Self::wrap_entry(e)))
            .collect()
    }

    /// Wraps a pool entry in a `ValidPoolTransaction` for external consumption.
    fn wrap_entry(entry: &PooledEntry<T>) -> Arc<ValidPoolTransaction<T>>
    where
        T: Clone,
    {
        let sender_id_val =
            u64::from_be_bytes(entry.id.sender.as_slice()[..8].try_into().unwrap_or_default());
        Arc::new(ValidPoolTransaction {
            transaction: entry.transaction.clone(),
            transaction_id: TransactionId::new(
                SenderId::from(sender_id_val),
                entry.id.nonce_sequence,
            ),
            propagate: true,
            timestamp: entry.timestamp,
            origin: entry.origin,
            authority_ids: None,
        })
    }
}

impl<T: EthPoolTransaction + Clone> Eip8130Pool<T> {
    /// Snapshots ready transactions using a zero base fee.
    pub fn best_transactions(&self) -> BestEip8130Transactions<T> {
        self.best_transactions_with_base_fee(0)
    }

    /// Snapshots the ready (executable) transactions across all sequences.
    ///
    /// A transaction is ready when its `is_pending` flag is set, meaning it
    /// forms part of a contiguous nonce chain from the on-chain nonce.
    /// Results are sorted by effective tip for the provided `base_fee`.
    pub fn best_transactions_with_base_fee(&self, base_fee: u64) -> BestEip8130Transactions<T> {
        let inner = self.inner.read();
        let mut ready = Vec::new();
        let mut by_hash = HashMap::new();

        for seq in inner.sequences.values() {
            for entry in seq.txs.values() {
                if entry.is_pending {
                    if entry.transaction.effective_tip_per_gas(base_fee).is_none() {
                        continue;
                    }
                    let tx = Self::wrap_entry(entry);
                    by_hash.insert(*tx.hash(), entry.id.clone());
                    ready.push(ReadyTx { id: entry.id.clone(), tx });
                }
            }
        }

        ready.sort_by(|a, b| {
            let a_prio = a.tx.transaction.effective_tip_per_gas(base_fee).unwrap_or_default();
            let b_prio = b.tx.transaction.effective_tip_per_gas(base_fee).unwrap_or_default();
            b_prio.cmp(&a_prio)
        });

        BestEip8130Transactions { ready: ready.into(), invalid: HashMap::new(), by_hash }
    }
}

struct ReadyTx<T: PoolTransaction> {
    id: Eip8130TxId,
    tx: Arc<ValidPoolTransaction<T>>,
}

/// Iterator over ready 2D-nonce transactions, sorted by priority.
///
/// Implements [`reth_transaction_pool::BestTransactions`] so it can be merged
/// with the standard pool's iterator during block building.
pub struct BestEip8130Transactions<T: PoolTransaction> {
    ready: VecDeque<ReadyTx<T>>,
    invalid: HashMap<Eip8130SequenceId, u64>,
    by_hash: HashMap<B256, Eip8130TxId>,
}

impl<T: PoolTransaction> core::fmt::Debug for BestEip8130Transactions<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BestEip8130Transactions")
            .field("ready_len", &self.ready.len())
            .field("invalid_len", &self.invalid.len())
            .field("by_hash_len", &self.by_hash.len())
            .finish()
    }
}

impl<T: EthPoolTransaction> Iterator for BestEip8130Transactions<T> {
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let ready = self.ready.pop_front()?;
            let seq_id = ready.id.sequence_id();
            if self
                .invalid
                .get(&seq_id)
                .is_some_and(|invalid_from| ready.id.nonce_sequence >= *invalid_from)
            {
                continue;
            }
            return Some(ready.tx);
        }
    }
}

impl<T: EthPoolTransaction> reth_transaction_pool::BestTransactions for BestEip8130Transactions<T> {
    fn mark_invalid(&mut self, transaction: &Self::Item, _kind: &InvalidPoolTransactionError) {
        let Some(id) = self.by_hash.get(transaction.hash()) else {
            return;
        };
        self.invalid
            .entry(id.sequence_id())
            .and_modify(|invalid_from| *invalid_from = (*invalid_from).min(id.nonce_sequence))
            .or_insert(id.nonce_sequence);
    }

    fn no_updates(&mut self) {}

    fn skip_blobs(&mut self) {}

    fn set_skip_blobs(&mut self, _skip: bool) {}
}

/// Errors returned by [`Eip8130Pool::add_transaction`].
#[derive(Debug, Clone)]
pub enum Eip8130PoolError {
    /// Transaction hash already exists in the pool.
    DuplicateHash(B256),
    /// The sequence lane `(sender, nonce_key)` has too many transactions.
    SequenceFull,
    /// A transaction at the same 2D nonce exists and the replacement does
    /// not meet the minimum price bump.
    ReplacementUnderpriced {
        /// The priority fee of the existing transaction.
        existing_fee: u128,
        /// The priority fee of the proposed replacement.
        new_fee: u128,
    },
    /// Pool has reached its maximum capacity.
    PoolFull,
    /// Account (sender or payer) already has the maximum number of
    /// transactions.
    AccountCapacityExceeded(Address),
}

impl core::fmt::Display for Eip8130PoolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DuplicateHash(hash) => write!(f, "duplicate transaction hash {hash}"),
            Self::SequenceFull => write!(f, "sequence lane is full"),
            Self::ReplacementUnderpriced { existing_fee, new_fee } => {
                write!(f, "replacement underpriced: existing_fee={existing_fee}, new_fee={new_fee}")
            }
            Self::PoolFull => write!(f, "2D nonce pool is full"),
            Self::AccountCapacityExceeded(account) => {
                write!(f, "account {account} exceeded per-account capacity")
            }
        }
    }
}

impl std::error::Error for Eip8130PoolError {}

impl reth_transaction_pool::error::PoolTransactionError for Eip8130PoolError {
    fn is_bad_transaction(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Shared handle to an [`Eip8130Pool`].
pub type SharedEip8130Pool<T> = Arc<Eip8130Pool<T>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BasePooledTransaction;
    use alloy_consensus::{Transaction, TxEip1559};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Signature, TxKind};
    use base_alloy_consensus::OpTransactionSigned;
    use base_alloy_consensus::OpTypedTransaction;
    use reth_primitives_traits::Recovered;

    type TestPool = Eip8130Pool<BasePooledTransaction>;

    fn cfg() -> Eip8130PoolConfig {
        Eip8130PoolConfig::default()
    }

    fn default_result(_: Address) -> TierCheckResult {
        TierCheckResult { tier: ThroughputTier::Default, cache_for: None }
    }

    fn locked_result(_: Address) -> TierCheckResult {
        TierCheckResult { tier: ThroughputTier::Locked, cache_for: None }
    }

    fn trusted_result(_: Address) -> TierCheckResult {
        TierCheckResult { tier: ThroughputTier::LockedTrustedBytecode, cache_for: None }
    }

    fn make_id(sender_byte: u8, nonce_key: u64, nonce_sequence: u64) -> Eip8130TxId {
        Eip8130TxId {
            sender: Address::repeat_byte(sender_byte),
            nonce_key: U256::from(nonce_key),
            nonce_sequence,
        }
    }

    fn make_slot(sender_byte: u8, nonce_key: u64) -> B256 {
        let mut buf = [0u8; 32];
        buf[0] = sender_byte;
        buf[24..32].copy_from_slice(&nonce_key.to_be_bytes());
        B256::from(buf)
    }

    fn make_tx(sender_byte: u8, nonce: u64, priority_fee: u128) -> BasePooledTransaction {
        let sender = Address::repeat_byte(sender_byte);
        let tx = TxEip1559 {
            chain_id: 1,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 1000,
            max_priority_fee_per_gas: priority_fee,
            to: TxKind::Call(Address::repeat_byte(0xFF)),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Default::default(),
        };
        let sig = Signature::new(
            U256::from(sender_byte as u64 * 1000 + nonce),
            U256::from(priority_fee),
            false,
        );
        let signed = OpTransactionSigned::new_unhashed(OpTypedTransaction::Eip1559(tx), sig);
        let recovered = Recovered::new_unchecked(signed, sender);
        let len = recovered.encode_2718_len();
        BasePooledTransaction::new(recovered, len)
    }

    fn add_self_pay(
        pool: &TestPool,
        id: Eip8130TxId,
        tx: BasePooledTransaction,
        slot: B256,
        check_tier: &dyn Fn(Address) -> TierCheckResult,
    ) -> Result<AddOutcome, Eip8130PoolError> {
        let payer = id.sender;
        pool.add_transaction(id, tx, payer, TransactionOrigin::External, slot, 0, check_tier)
    }

    fn add_sponsored(
        pool: &TestPool,
        id: Eip8130TxId,
        tx: BasePooledTransaction,
        payer: Address,
        slot: B256,
        check_tier: &dyn Fn(Address) -> TierCheckResult,
    ) -> Result<AddOutcome, Eip8130PoolError> {
        pool.add_transaction(id, tx, payer, TransactionOrigin::External, slot, 0, check_tier)
    }

    // ------------------------------------------------------------------ //
    //  Basic identity / routing
    // ------------------------------------------------------------------ //

    fn is_nonzero_key(nonce_key: U256) -> bool {
        !nonce_key.is_zero()
    }

    #[test]
    fn nonce_key_classification() {
        assert!(!is_nonzero_key(U256::ZERO));
        assert!(is_nonzero_key(U256::from(1)));
        assert!(is_nonzero_key(U256::from(u64::MAX)));
    }

    #[test]
    fn seq_id_construction() {
        let id = make_id(0x01, 1, 5);
        let seq = id.sequence_id();
        assert_eq!(seq.sender, Address::repeat_byte(0x01));
        assert_eq!(seq.nonce_key, U256::from(1));
    }

    // ------------------------------------------------------------------ //
    //  Empty pool
    // ------------------------------------------------------------------ //

    #[test]
    fn empty_pool_properties() {
        let pool = TestPool::new();
        assert!(pool.is_empty());
        assert_eq!(pool.len(), 0);
        assert_eq!(pool.pending_and_queued_count(), (0, 0));
        assert!(pool.all_hashes().is_empty());
    }

    // ------------------------------------------------------------------ //
    //  add_transaction — basic
    // ------------------------------------------------------------------ //

    #[test]
    fn add_single_transaction() {
        let pool = TestPool::new();
        let id = make_id(0x01, 1, 0);
        let tx = make_tx(0x01, 0, 10);
        let hash = *tx.hash();
        let slot = make_slot(0x01, 1);

        add_self_pay(&pool, id, tx, slot, &default_result).unwrap();

        assert_eq!(pool.len(), 1);
        assert!(pool.contains(&hash));
        assert!(!pool.is_empty());
    }

    #[test]
    fn add_duplicate_hash_rejected() {
        let pool = TestPool::new();
        let tx = make_tx(0x01, 0, 10);
        let id = make_id(0x01, 1, 0);
        let slot = make_slot(0x01, 1);

        add_self_pay(&pool, id.clone(), tx.clone(), slot, &default_result).unwrap();

        let result = add_self_pay(&pool, id, tx, slot, &default_result);
        assert!(matches!(result, Err(Eip8130PoolError::DuplicateHash(_))));
    }

    #[test]
    fn replacement_underpriced_rejected() {
        let pool = TestPool::new();
        let slot = make_slot(0x01, 1);

        let tx1 = make_tx(0x01, 0, 100);
        let id1 = make_id(0x01, 1, 0);
        add_self_pay(&pool, id1, tx1, slot, &default_result).unwrap();

        // Same nonce but fee doesn't meet the 10% bump — rejected.
        let tx2 = make_tx(0x01, 100, 105);
        let id2 = make_id(0x01, 1, 0);
        let result = add_self_pay(&pool, id2, tx2, slot, &default_result);
        assert!(matches!(result, Err(Eip8130PoolError::ReplacementUnderpriced { .. })));
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn replacement_with_sufficient_bump_succeeds() {
        let pool = TestPool::new();
        let slot = make_slot(0x01, 1);

        let tx1 = make_tx(0x01, 0, 100);
        let hash1 = *tx1.hash();
        let id1 = make_id(0x01, 1, 0);
        add_self_pay(&pool, id1, tx1, slot, &default_result).unwrap();

        // 110% of 100 = 110 — meets the 10% bump.
        let tx2 = make_tx(0x01, 100, 110);
        let hash2 = *tx2.hash();
        let id2 = make_id(0x01, 1, 0);
        let result = add_self_pay(&pool, id2, tx2, slot, &default_result).unwrap();
        assert_eq!(result, AddOutcome::Replaced);
        assert_eq!(pool.len(), 1);
        assert!(!pool.contains(&hash1));
        assert!(pool.contains(&hash2));
    }

    #[test]
    fn sequence_full_rejected() {
        let c = cfg();
        let pool = TestPool::new();
        let slot = make_slot(0x01, 1);

        for seq in 0..c.max_txs_per_sequence as u64 {
            let tx = make_tx(0x01, seq, 10);
            let id = make_id(0x01, 1, seq);
            add_self_pay(&pool, id, tx, slot, &trusted_result).unwrap();
        }

        let tx = make_tx(0x01, c.max_txs_per_sequence as u64, 10);
        let id = make_id(0x01, 1, c.max_txs_per_sequence as u64);
        let result = add_self_pay(&pool, id, tx, slot, &trusted_result);
        assert!(matches!(result, Err(Eip8130PoolError::SequenceFull)));
    }

    // ------------------------------------------------------------------ //
    //  Dual-counter semantics
    // ------------------------------------------------------------------ //

    #[test]
    fn self_pay_increments_payer_only() {
        let pool = TestPool::new();
        let sender = Address::repeat_byte(0x01);

        let tx = make_tx(0x01, 0, 10);
        let id = make_id(0x01, 1, 0);
        let slot = make_slot(0x01, 1);
        add_self_pay(&pool, id, tx, slot, &default_result).unwrap();

        assert_eq!(pool.sender_tx_count(&sender), 0, "self-pay should not bump sender counter");
        assert_eq!(pool.payer_tx_count(&sender), 1, "self-pay should bump payer counter");
    }

    #[test]
    fn sponsored_increments_sender_and_payer() {
        let pool = TestPool::new();
        let sender = Address::repeat_byte(0x01);
        let payer = Address::repeat_byte(0xBB);

        let tx = make_tx(0x01, 0, 10);
        let id = make_id(0x01, 1, 0);
        let slot = make_slot(0x01, 1);
        add_sponsored(&pool, id, tx, payer, slot, &default_result).unwrap();

        assert_eq!(pool.sender_tx_count(&sender), 1);
        assert_eq!(pool.payer_tx_count(&sender), 0, "sender should not get payer bump");
        assert_eq!(pool.payer_tx_count(&payer), 1);
        assert_eq!(pool.sender_tx_count(&payer), 0, "payer should not get sender bump");
    }

    // ------------------------------------------------------------------ //
    //  Per-account payer limit (self-pay)
    // ------------------------------------------------------------------ //

    #[test]
    fn payer_limit_blocks_excess_self_pay() {
        let c = cfg();
        let pool = TestPool::new();

        for key in 1..=c.default_max_payer_txs as u64 {
            let tx = make_tx(0x01, key, 10);
            let id = make_id(0x01, key, 0);
            let slot = make_slot(0x01, key);
            add_self_pay(&pool, id, tx, slot, &default_result).unwrap();
        }

        let key = c.default_max_payer_txs as u64 + 1;
        let tx = make_tx(0x01, key, 10);
        let id = make_id(0x01, key, 0);
        let slot = make_slot(0x01, key);
        let result = add_self_pay(&pool, id, tx, slot, &default_result);
        assert!(matches!(result, Err(Eip8130PoolError::AccountCapacityExceeded(_))));
    }

    #[test]
    fn payer_limit_freed_after_removal() {
        let c = cfg();
        let pool = TestPool::new();

        let mut hashes = Vec::new();
        for key in 1..=c.default_max_payer_txs as u64 {
            let tx = make_tx(0x01, key, 10);
            hashes.push(*tx.hash());
            let id = make_id(0x01, key, 0);
            let slot = make_slot(0x01, key);
            add_self_pay(&pool, id, tx, slot, &default_result).unwrap();
        }

        pool.remove_transaction(&hashes[0]);

        let key = c.default_max_payer_txs as u64 + 1;
        let tx = make_tx(0x01, key, 10);
        let id = make_id(0x01, key, 0);
        let slot = make_slot(0x01, key);
        add_self_pay(&pool, id, tx, slot, &default_result)
            .expect("should succeed after freeing a slot");
    }

    #[test]
    fn payer_limit_independent_across_accounts() {
        let c = cfg();
        let pool = TestPool::new();

        for key in 1..=c.default_max_payer_txs as u64 {
            let tx = make_tx(0x01, key, 10);
            let id = make_id(0x01, key, 0);
            let slot = make_slot(0x01, key);
            add_self_pay(&pool, id, tx, slot, &default_result).unwrap();
        }

        let tx = make_tx(0x02, 1, 10);
        let id = make_id(0x02, 1, 0);
        let slot = make_slot(0x02, 1);
        add_self_pay(&pool, id, tx, slot, &default_result)
            .expect("different account should not be affected");
    }

    // ------------------------------------------------------------------ //
    //  Per-account sender limit (sponsored)
    // ------------------------------------------------------------------ //

    #[test]
    fn sender_limit_blocks_excess_sponsored() {
        let c = cfg();
        let pool = TestPool::new();

        for key in 1..=c.default_max_sender_txs as u64 {
            let payer = Address::repeat_byte(key as u8 + 0x80);
            let tx = make_tx(0x01, key, 10);
            let id = make_id(0x01, key, 0);
            let slot = make_slot(0x01, key);
            add_sponsored(&pool, id, tx, payer, slot, &default_result).unwrap();
        }

        let key = c.default_max_sender_txs as u64 + 1;
        let payer = Address::repeat_byte(key as u8 + 0x80);
        let tx = make_tx(0x01, key, 10);
        let id = make_id(0x01, key, 0);
        let slot = make_slot(0x01, key);
        let result = add_sponsored(&pool, id, tx, payer, slot, &default_result);
        assert!(
            matches!(result, Err(Eip8130PoolError::AccountCapacityExceeded(addr)) if addr == Address::repeat_byte(0x01)),
            "sender should be rejected"
        );
    }

    // ------------------------------------------------------------------ //
    //  Payer limit for a shared payer across senders
    // ------------------------------------------------------------------ //

    #[test]
    fn shared_payer_limit_blocks_excess() {
        let c = cfg();
        let pool = TestPool::new();
        let payer = Address::repeat_byte(0xBB);

        for key in 1..=c.default_max_payer_txs as u64 {
            let sender_byte = key as u8;
            let tx = make_tx(sender_byte, 1, 10);
            let id = make_id(sender_byte, 1, 0);
            let slot = make_slot(sender_byte, 1);
            add_sponsored(&pool, id, tx, payer, slot, &default_result).unwrap();
        }

        let over = c.default_max_payer_txs as u8 + 1;
        let tx = make_tx(over, 1, 10);
        let id = make_id(over, 1, 0);
        let slot = make_slot(over, 1);
        let result = add_sponsored(&pool, id, tx, payer, slot, &default_result);
        assert!(
            matches!(result, Err(Eip8130PoolError::AccountCapacityExceeded(addr)) if addr == payer),
            "payer at limit should block new txs"
        );
    }

    // ------------------------------------------------------------------ //
    //  3-tier throughput
    // ------------------------------------------------------------------ //

    #[test]
    fn trusted_tier_allows_more_self_pay() {
        let c = cfg();
        let pool = TestPool::new();

        for key in 1..=c.default_max_payer_txs as u64 {
            let tx = make_tx(0x01, key, 10);
            let id = make_id(0x01, key, 0);
            let slot = make_slot(0x01, key);
            add_self_pay(&pool, id, tx, slot, &trusted_result).unwrap();
        }

        let key = c.default_max_payer_txs as u64 + 1;
        let tx = make_tx(0x01, key, 10);
        let id = make_id(0x01, key, 0);
        let slot = make_slot(0x01, key);
        add_self_pay(&pool, id, tx, slot, &trusted_result)
            .expect("trusted account should accept more than the default payer limit");
    }

    #[test]
    fn locked_tier_increases_sender_limit_only() {
        let c = cfg();
        let pool = TestPool::new();

        for key in 1..=c.default_max_sender_txs as u64 {
            let payer = Address::repeat_byte(key as u8 + 0x80);
            let tx = make_tx(0x01, key, 10);
            let id = make_id(0x01, key, 0);
            let slot = make_slot(0x01, key);
            add_sponsored(&pool, id, tx, payer, slot, &locked_result).unwrap();
        }

        let key = c.default_max_sender_txs as u64 + 1;
        let payer = Address::repeat_byte(key as u8 + 0x80);
        let tx = make_tx(0x01, key, 10);
        let id = make_id(0x01, key, 0);
        let slot = make_slot(0x01, key);
        add_sponsored(&pool, id, tx, payer, slot, &locked_result)
            .expect("locked sender should accept more sponsored txs than default");
    }

    #[test]
    fn locked_tier_does_not_increase_payer_limit() {
        let c = cfg();
        let pool = TestPool::new();

        for key in 1..=c.default_max_payer_txs as u64 {
            let tx = make_tx(0x01, key, 10);
            let id = make_id(0x01, key, 0);
            let slot = make_slot(0x01, key);
            add_self_pay(&pool, id, tx, slot, &locked_result).unwrap();
        }

        let key = c.default_max_payer_txs as u64 + 1;
        let tx = make_tx(0x01, key, 10);
        let id = make_id(0x01, key, 0);
        let slot = make_slot(0x01, key);
        let result = add_self_pay(&pool, id, tx, slot, &locked_result);
        assert!(
            matches!(result, Err(Eip8130PoolError::AccountCapacityExceeded(_))),
            "Locked tier should not increase payer limit"
        );
    }

    #[test]
    fn trusted_tier_allows_more_for_payer() {
        let c = cfg();
        let pool = TestPool::new();
        let payer = Address::repeat_byte(0xBB);

        for key in 1..=c.default_max_payer_txs as u64 {
            let sender_byte = key as u8;
            let tx = make_tx(sender_byte, 1, 10);
            let id = make_id(sender_byte, 1, 0);
            let slot = make_slot(sender_byte, 1);
            add_sponsored(&pool, id, tx, payer, slot, &trusted_result).unwrap();
        }

        let over = c.default_max_payer_txs as u8 + 1;
        let tx = make_tx(over, 1, 10);
        let id = make_id(over, 1, 0);
        let slot = make_slot(over, 1);
        add_sponsored(&pool, id, tx, payer, slot, &trusted_result)
            .expect("trusted payer should accept more than the default limit");
    }

    #[test]
    fn tier_defaults_ordered() {
        let c = cfg();
        assert!(c.default_max_sender_txs < c.locked_max_sender_txs);
        assert!(c.locked_max_sender_txs <= c.trusted_max_sender_txs);
        assert!(c.default_max_payer_txs < c.trusted_max_payer_txs);
    }

    #[test]
    fn pool_full_rejected() {
        let c = cfg();
        let pool = TestPool::new();

        fn sender_from_index(i: usize) -> Address {
            let mut bytes = [0u8; 20];
            bytes[12..20].copy_from_slice(&(i as u64).to_be_bytes());
            Address::from(bytes)
        }

        fn make_tx_for_sender(
            sender: Address,
            nonce: u64,
            priority_fee: u128,
        ) -> BasePooledTransaction {
            let tx = TxEip1559 {
                chain_id: 1,
                nonce,
                gas_limit: 21_000,
                max_fee_per_gas: 1000,
                max_priority_fee_per_gas: priority_fee,
                to: TxKind::Call(Address::repeat_byte(0xFF)),
                value: U256::ZERO,
                access_list: Default::default(),
                input: Default::default(),
            };
            let sig = Signature::new(
                U256::from(nonce.saturating_add(1)),
                U256::from(priority_fee),
                false,
            );
            let signed = OpTransactionSigned::new_unhashed(OpTypedTransaction::Eip1559(tx), sig);
            let recovered = Recovered::new_unchecked(signed, sender);
            let len = recovered.encode_2718_len();
            BasePooledTransaction::new(recovered, len)
        }

        fn make_slot_for(sender: Address, nonce_key: u64) -> B256 {
            let mut buf = [0u8; 32];
            buf[..20].copy_from_slice(sender.as_slice());
            buf[24..32].copy_from_slice(&nonce_key.to_be_bytes());
            B256::from(buf)
        }

        for i in 0..c.max_pool_size {
            let sender = sender_from_index(i);
            let nonce_key = i as u64 + 1;
            let tx = make_tx_for_sender(sender, i as u64, 10);
            let id = Eip8130TxId { sender, nonce_key: U256::from(nonce_key), nonce_sequence: 0 };
            let slot = make_slot_for(sender, nonce_key);
            add_self_pay(&pool, id, tx, slot, &trusted_result).unwrap();
        }

        assert_eq!(pool.len(), c.max_pool_size);

        // With eviction, adding a higher-priority tx evicts the lowest.
        let sender = sender_from_index(c.max_pool_size + 1);
        let tx = make_tx_for_sender(sender, 9999, 20);
        let id = Eip8130TxId { sender, nonce_key: U256::from(9999), nonce_sequence: 0 };
        let slot = make_slot_for(sender, 9999);
        let result = add_self_pay(&pool, id, tx, slot, &trusted_result);
        assert!(result.is_ok(), "should succeed by evicting lowest-priority tx");
        assert_eq!(pool.len(), c.max_pool_size, "pool size should stay at max");
    }

    #[test]
    fn tier_cache_expires() {
        let config = Eip8130PoolConfig {
            default_max_payer_txs: 2,
            trusted_max_payer_txs: 10,
            tier_cache_ttl: Duration::from_millis(50),
            ..Eip8130PoolConfig::default()
        };
        let pool = Eip8130Pool::<BasePooledTransaction>::with_config(config);

        for key in 1..=3u64 {
            let tx = make_tx(0x01, key, 10);
            let id = make_id(0x01, key, 0);
            let slot = make_slot(0x01, key);
            add_self_pay(&pool, id, tx, slot, &trusted_result).unwrap();
        }

        {
            let inner = pool.inner.read();
            assert_eq!(
                inner.account_tiers.get(&Address::repeat_byte(0x01)).unwrap().tier,
                ThroughputTier::LockedTrustedBytecode,
            );
        }

        std::thread::sleep(Duration::from_millis(60));

        let tx = make_tx(0x01, 4, 10);
        let id = make_id(0x01, 4, 0);
        let slot = make_slot(0x01, 4);
        let result = add_self_pay(&pool, id, tx, slot, &default_result);
        assert!(
            matches!(result, Err(Eip8130PoolError::AccountCapacityExceeded(_))),
            "after TTL expiry, default tier check should reject at the default limit"
        );
    }

    #[test]
    fn tier_cache_uses_min_of_cache_for_and_config_ttl() {
        let config = Eip8130PoolConfig {
            default_max_payer_txs: 2,
            trusted_max_payer_txs: 10,
            tier_cache_ttl: Duration::from_secs(600),
            ..Eip8130PoolConfig::default()
        };
        let pool = Eip8130Pool::<BasePooledTransaction>::with_config(config);

        let short_ttl = |_: Address| -> TierCheckResult {
            TierCheckResult {
                tier: ThroughputTier::LockedTrustedBytecode,
                cache_for: Some(Duration::from_millis(40)),
            }
        };

        for key in 1..=3u64 {
            let tx = make_tx(0x01, key, 10);
            let id = make_id(0x01, key, 0);
            let slot = make_slot(0x01, key);
            add_self_pay(&pool, id, tx, slot, &short_ttl).unwrap();
        }

        std::thread::sleep(Duration::from_millis(60));

        let tx = make_tx(0x01, 4, 10);
        let id = make_id(0x01, 4, 0);
        let slot = make_slot(0x01, 4);
        let result = add_self_pay(&pool, id, tx, slot, &default_result);
        assert!(
            matches!(result, Err(Eip8130PoolError::AccountCapacityExceeded(_))),
            "cache_for should override longer config TTL"
        );
    }

    // ------------------------------------------------------------------ //
    //  remove_transaction
    // ------------------------------------------------------------------ //

    #[test]
    fn remove_transaction_cleans_up() {
        let pool = TestPool::new();
        let tx = make_tx(0x01, 0, 10);
        let hash = *tx.hash();
        let id = make_id(0x01, 1, 0);
        let slot = make_slot(0x01, 1);

        add_self_pay(&pool, id, tx, slot, &default_result).unwrap();
        assert_eq!(pool.len(), 1);

        let removed_id = pool.remove_transaction(&hash);
        assert!(removed_id.is_some());
        assert!(pool.is_empty());
        assert!(!pool.contains(&hash));

        let inner = pool.inner.read();
        assert!(inner.slot_to_seq.is_empty(), "slot_to_seq should be cleaned up");
    }

    #[test]
    fn remove_nonexistent_returns_none() {
        let pool = TestPool::new();
        assert!(pool.remove_transaction(&B256::ZERO).is_none());
    }

    #[test]
    fn remove_transactions_batch() {
        let pool = TestPool::new();
        let slot = make_slot(0x01, 1);
        let mut hashes = Vec::new();

        for seq in 0..3u64 {
            let tx = make_tx(0x01, seq, 10);
            hashes.push(*tx.hash());
            let id = make_id(0x01, 1, seq);
            add_self_pay(&pool, id, tx, slot, &default_result).unwrap();
        }

        let removed = pool.remove_transactions(&hashes[..2]);
        assert_eq!(removed.len(), 2);
        assert_eq!(pool.len(), 1);
        assert!(pool.contains(&hashes[2]));
    }

    #[test]
    fn remove_sponsored_tx_decrements_correct_counters() {
        let pool = TestPool::new();
        let sender = Address::repeat_byte(0x01);
        let payer = Address::repeat_byte(0xBB);

        let tx = make_tx(0x01, 0, 10);
        let hash = *tx.hash();
        let id = make_id(0x01, 1, 0);
        let slot = make_slot(0x01, 1);
        add_sponsored(&pool, id, tx, payer, slot, &default_result).unwrap();

        assert_eq!(pool.sender_tx_count(&sender), 1);
        assert_eq!(pool.payer_tx_count(&payer), 1);

        pool.remove_transaction(&hash);

        assert_eq!(pool.sender_tx_count(&sender), 0);
        assert_eq!(pool.payer_tx_count(&payer), 0);
    }

    #[test]
    fn remove_self_pay_tx_decrements_payer_only() {
        let pool = TestPool::new();
        let sender = Address::repeat_byte(0x01);

        let tx = make_tx(0x01, 0, 10);
        let hash = *tx.hash();
        let id = make_id(0x01, 1, 0);
        let slot = make_slot(0x01, 1);
        add_self_pay(&pool, id, tx, slot, &default_result).unwrap();

        pool.remove_transaction(&hash);

        assert_eq!(pool.sender_tx_count(&sender), 0);
        assert_eq!(pool.payer_tx_count(&sender), 0);
    }

    // ------------------------------------------------------------------ //
    //  update_sequence_nonce
    // ------------------------------------------------------------------ //

    #[test]
    fn update_sequence_nonce_prunes_stale() {
        let pool = TestPool::new();
        let slot = make_slot(0x01, 1);

        for seq in 0..5u64 {
            let tx = make_tx(0x01, seq, 10);
            let id = make_id(0x01, 1, seq);
            add_self_pay(&pool, id, tx, slot, &default_result).unwrap();
        }
        assert_eq!(pool.len(), 5);

        let seq_id =
            Eip8130SequenceId { sender: Address::repeat_byte(0x01), nonce_key: U256::from(1) };
        let pruned = pool.update_sequence_nonce(&seq_id, 3);
        assert_eq!(pruned.len(), 3);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn update_sequence_nonce_removes_empty_sequence() {
        let pool = TestPool::new();
        let slot = make_slot(0x01, 1);

        let tx = make_tx(0x01, 0, 10);
        let id = make_id(0x01, 1, 0);
        add_self_pay(&pool, id, tx, slot, &default_result).unwrap();

        let seq_id =
            Eip8130SequenceId { sender: Address::repeat_byte(0x01), nonce_key: U256::from(1) };
        pool.update_sequence_nonce(&seq_id, 1);
        assert!(pool.is_empty());

        let inner = pool.inner.read();
        assert!(inner.sequences.is_empty());
        assert!(inner.slot_to_seq.is_empty());
    }

    // ------------------------------------------------------------------ //
    //  pending / queued classification
    // ------------------------------------------------------------------ //

    #[test]
    fn pending_and_queued_with_gap() {
        let pool = TestPool::new();
        let slot = make_slot(0x01, 1);

        for seq in [0, 1, 3, 4] {
            let tx = make_tx(0x01, seq, 10);
            let id = make_id(0x01, 1, seq);
            add_self_pay(&pool, id, tx, slot, &default_result).unwrap();
        }

        let (pending, queued) = pool.pending_and_queued_count();
        assert_eq!(pending, 2, "nonces 0,1 are contiguous from next_nonce=0");
        assert_eq!(queued, 2, "nonces 3,4 have a gap");
    }

    // ------------------------------------------------------------------ //
    //  best_transactions
    // ------------------------------------------------------------------ //

    #[test]
    fn best_transactions_ordered_by_priority() {
        let pool = TestPool::new();

        let tx_low = make_tx(0x01, 0, 5);
        let id_low = make_id(0x01, 1, 0);
        let slot1 = make_slot(0x01, 1);
        add_self_pay(&pool, id_low, tx_low, slot1, &default_result).unwrap();

        let tx_high = make_tx(0x02, 0, 50);
        let id_high = make_id(0x02, 2, 0);
        let slot2 = make_slot(0x02, 2);
        add_self_pay(&pool, id_high, tx_high, slot2, &default_result).unwrap();

        let mut best = pool.best_transactions();
        let first = best.next().unwrap();
        let second = best.next().unwrap();

        let first_prio = first.transaction.max_priority_fee_per_gas().unwrap_or_default();
        let second_prio = second.transaction.max_priority_fee_per_gas().unwrap_or_default();
        assert!(first_prio >= second_prio, "first={first_prio}, second={second_prio}");
        assert!(best.next().is_none());
    }

    #[test]
    fn best_transactions_skips_gapped() {
        let pool = TestPool::new();
        let slot = make_slot(0x01, 1);

        let tx = make_tx(0x01, 2, 10);
        let id = make_id(0x01, 1, 2);
        add_self_pay(&pool, id, tx, slot, &default_result).unwrap();

        let mut best = pool.best_transactions();
        assert!(best.next().is_none(), "nonce 2 has a gap from next_nonce=0");
    }

    #[test]
    fn best_transactions_mark_invalid_skips_sender() {
        let pool = TestPool::new();

        let tx1 = make_tx(0x01, 0, 50);
        let id1 = make_id(0x01, 1, 0);
        let slot1 = make_slot(0x01, 1);
        add_self_pay(&pool, id1, tx1, slot1, &default_result).unwrap();

        let tx2 = make_tx(0x01, 1, 40);
        let id2 = make_id(0x01, 1, 1);
        add_self_pay(&pool, id2, tx2, slot1, &default_result).unwrap();

        let tx3 = make_tx(0x02, 0, 30);
        let id3 = make_id(0x02, 2, 0);
        let slot2 = make_slot(0x02, 2);
        add_self_pay(&pool, id3, tx3, slot2, &default_result).unwrap();

        let mut best = pool.best_transactions();
        let first = best.next().unwrap();
        assert_eq!(first.sender(), Address::repeat_byte(0x01));

        use reth_transaction_pool::BestTransactions;
        let err = InvalidPoolTransactionError::Other(Box::new(Eip8130PoolError::PoolFull));
        best.mark_invalid(&first, &err);

        let next = best.next().unwrap();
        assert_eq!(
            next.sender(),
            Address::repeat_byte(0x02),
            "should skip remaining txs from sender 0x01"
        );
        assert!(best.next().is_none());
    }

    #[test]
    fn best_transactions_mark_invalid_keeps_other_lanes_for_same_sender() {
        let pool = TestPool::new();

        let lane_one_slot = make_slot(0x01, 1);
        let lane_two_slot = make_slot(0x01, 2);

        let tx1 = make_tx(0x01, 0, 50);
        let id1 = make_id(0x01, 1, 0);
        add_self_pay(&pool, id1, tx1, lane_one_slot, &default_result).unwrap();

        let tx2 = make_tx(0x01, 0, 40);
        let id2 = make_id(0x01, 2, 0);
        add_self_pay(&pool, id2, tx2, lane_two_slot, &default_result).unwrap();

        let mut best = pool.best_transactions();
        let first = best.next().unwrap();
        assert_eq!(first.sender(), Address::repeat_byte(0x01));

        use reth_transaction_pool::BestTransactions;
        let err = InvalidPoolTransactionError::Other(Box::new(Eip8130PoolError::PoolFull));
        best.mark_invalid(&first, &err);

        let next = best.next().unwrap();
        assert_eq!(next.sender(), Address::repeat_byte(0x01));
        assert_ne!(next.hash(), first.hash(), "different nonce lanes should remain eligible");
    }

    #[test]
    fn best_transactions_with_base_fee_filters_ineligible_transactions() {
        let pool = TestPool::new();

        let tx = make_tx(0x01, 0, 10);
        let id = make_id(0x01, 1, 0);
        let slot = make_slot(0x01, 1);
        add_self_pay(&pool, id, tx, slot, &default_result).unwrap();

        let mut best = pool.best_transactions_with_base_fee(2_000);
        assert!(
            best.next().is_none(),
            "tx with max_fee_per_gas below base fee should be excluded from ready set"
        );
    }

    // ------------------------------------------------------------------ //
    //  slot_to_seq reverse index
    // ------------------------------------------------------------------ //

    #[test]
    fn slot_to_seq_lookup() {
        let pool = TestPool::new();
        let slot = make_slot(0x01, 1);
        let tx = make_tx(0x01, 0, 10);
        let id = make_id(0x01, 1, 0);

        add_self_pay(&pool, id, tx, slot, &default_result).unwrap();

        let seq_id = pool.seq_id_for_slot(&slot).unwrap();
        assert_eq!(seq_id.sender, Address::repeat_byte(0x01));
        assert_eq!(seq_id.nonce_key, U256::from(1));
    }

    // ------------------------------------------------------------------ //
    //  Counter queries across lanes
    // ------------------------------------------------------------------ //

    #[test]
    fn payer_count_across_lanes() {
        let pool = TestPool::new();

        let tx1 = make_tx(0x01, 0, 10);
        let id1 = make_id(0x01, 1, 0);
        let slot1 = make_slot(0x01, 1);
        add_self_pay(&pool, id1, tx1, slot1, &default_result).unwrap();

        let tx2 = make_tx(0x01, 1, 10);
        let id2 = make_id(0x01, 2, 0);
        let slot2 = make_slot(0x01, 2);
        add_self_pay(&pool, id2, tx2, slot2, &default_result).unwrap();

        assert_eq!(pool.payer_tx_count(&Address::repeat_byte(0x01)), 2);
        assert_eq!(pool.payer_tx_count(&Address::repeat_byte(0x02)), 0);
    }

    // ------------------------------------------------------------------ //
    //  Promotion on gap-fill
    // ------------------------------------------------------------------ //

    #[test]
    fn filling_gap_promotes_descendants() {
        let pool = TestPool::new();
        let slot = make_slot(0x01, 1);

        // Insert nonces 0, 2, 3 — gap at 1.
        for seq in [0, 2, 3] {
            let tx = make_tx(0x01, seq, 10);
            let id = make_id(0x01, 1, seq);
            add_self_pay(&pool, id, tx, slot, &default_result).unwrap();
        }

        let (pending, queued) = pool.pending_and_queued_count();
        assert_eq!(pending, 1, "only nonce 0 should be pending");
        assert_eq!(queued, 2, "nonces 2,3 should be queued");

        // Fill the gap at nonce 1.
        let tx1 = make_tx(0x01, 1, 10);
        let id1 = make_id(0x01, 1, 1);
        add_self_pay(&pool, id1, tx1, slot, &default_result).unwrap();

        let (pending, queued) = pool.pending_and_queued_count();
        assert_eq!(pending, 4, "all 4 nonces should now be pending");
        assert_eq!(queued, 0);
    }

    // ------------------------------------------------------------------ //
    //  Demotion on mid-chain removal
    // ------------------------------------------------------------------ //

    #[test]
    fn removing_mid_chain_demotes_descendants() {
        let pool = TestPool::new();
        let slot = make_slot(0x01, 1);

        let mut hashes = Vec::new();
        for seq in 0..4u64 {
            let tx = make_tx(0x01, seq, 10);
            hashes.push(*tx.hash());
            let id = make_id(0x01, 1, seq);
            add_self_pay(&pool, id, tx, slot, &default_result).unwrap();
        }

        let (pending, _) = pool.pending_and_queued_count();
        assert_eq!(pending, 4);

        // Remove nonce 1 — creates a gap.
        pool.remove_transaction(&hashes[1]);

        let (pending, queued) = pool.pending_and_queued_count();
        assert_eq!(pending, 1, "only nonce 0 should remain pending");
        assert_eq!(queued, 2, "nonces 2,3 should be demoted to queued");
    }

    // ------------------------------------------------------------------ //
    //  Priority-based eviction
    // ------------------------------------------------------------------ //

    #[test]
    fn eviction_removes_lowest_priority_queued_first() {
        let config = Eip8130PoolConfig {
            max_pool_size: 3,
            max_txs_per_sequence: 16,
            default_max_payer_txs: 128,
            trusted_max_payer_txs: 128,
            ..Eip8130PoolConfig::default()
        };
        let pool = Eip8130Pool::<BasePooledTransaction>::with_config(config);

        // nonce 0 (pending, fee=50), nonce 2 (queued, fee=5), nonce 3 (queued, fee=100)
        let slot = make_slot(0x01, 1);
        let tx0 = make_tx(0x01, 0, 50);
        let id0 = make_id(0x01, 1, 0);
        add_self_pay(&pool, id0, tx0, slot, &trusted_result).unwrap();

        let tx2 = make_tx(0x01, 200, 5);
        let id2 = make_id(0x01, 1, 2);
        add_self_pay(&pool, id2, tx2, slot, &trusted_result).unwrap();

        let tx3 = make_tx(0x01, 300, 100);
        let id3 = make_id(0x01, 1, 3);
        add_self_pay(&pool, id3, tx3, slot, &trusted_result).unwrap();

        assert_eq!(pool.len(), 3);
        let (pending, queued) = pool.pending_and_queued_count();
        assert_eq!(pending, 1);
        assert_eq!(queued, 2);

        // Adding a 4th tx should evict the lowest-priority queued (fee=5).
        let tx_new = make_tx(0x02, 0, 20);
        let id_new = make_id(0x02, 2, 0);
        let slot2 = make_slot(0x02, 2);
        add_self_pay(&pool, id_new, tx_new, slot2, &trusted_result).unwrap();

        assert_eq!(pool.len(), 3, "pool should remain at max size after eviction");
    }

    // ------------------------------------------------------------------ //
    //  Price bump helper
    // ------------------------------------------------------------------ //

    #[test]
    fn price_bump_math() {
        assert!(Eip8130Pool::<BasePooledTransaction>::meets_price_bump(100, 110, 10));
        assert!(!Eip8130Pool::<BasePooledTransaction>::meets_price_bump(100, 109, 10));
        assert!(Eip8130Pool::<BasePooledTransaction>::meets_price_bump(0, 0, 10));
        assert!(Eip8130Pool::<BasePooledTransaction>::meets_price_bump(0, 1, 10));
    }

    // ------------------------------------------------------------------ //
    //  Nonce update promotes survivors
    // ------------------------------------------------------------------ //

    #[test]
    fn update_nonce_promotes_survivors() {
        let pool = TestPool::new();
        let slot = make_slot(0x01, 1);

        // Insert nonces 0, 1, 4, 5 — gap at 2-3.
        for seq in [0, 1, 4, 5] {
            let tx = make_tx(0x01, seq, 10);
            let id = make_id(0x01, 1, seq);
            add_self_pay(&pool, id, tx, slot, &default_result).unwrap();
        }

        let (pending, queued) = pool.pending_and_queued_count();
        assert_eq!(pending, 2, "nonces 0,1 pending");
        assert_eq!(queued, 2, "nonces 4,5 queued");

        // Advance nonce to 4 — removes 0, 1; promotes 4, 5.
        let seq_id =
            Eip8130SequenceId { sender: Address::repeat_byte(0x01), nonce_key: U256::from(1) };
        let pruned = pool.update_sequence_nonce(&seq_id, 4);
        assert_eq!(pruned.len(), 2);
        assert_eq!(pool.len(), 2);

        let (pending, queued) = pool.pending_and_queued_count();
        assert_eq!(pending, 2, "nonces 4,5 should now be pending");
        assert_eq!(queued, 0);
    }

    #[test]
    fn sweep_expired_removes_only_expired() {
        let pool = TestPool::new();
        let tier = &default_result;

        let sender_byte = 0xAA;
        let sender = Address::repeat_byte(sender_byte);
        let slot = B256::ZERO;

        let id0 = Eip8130TxId { sender, nonce_key: U256::from(1), nonce_sequence: 0 };
        let id1 = Eip8130TxId { sender, nonce_key: U256::from(1), nonce_sequence: 1 };
        let id2 = Eip8130TxId { sender, nonce_key: U256::from(1), nonce_sequence: 2 };

        let tx0 = make_tx(sender_byte, 0, 10);
        let tx1 = make_tx(sender_byte, 1, 10);
        let tx2 = make_tx(sender_byte, 2, 10);

        // tx0: no expiry (0), tx1: expires at 1000, tx2: expires at 2000
        pool.add_transaction(id0, tx0, sender, TransactionOrigin::External, slot, 0, tier).unwrap();
        pool.add_transaction(id1, tx1, sender, TransactionOrigin::External, slot, 1000, tier)
            .unwrap();
        pool.add_transaction(id2, tx2, sender, TransactionOrigin::External, slot, 2000, tier)
            .unwrap();

        assert_eq!(pool.len(), 3);

        // At time 500, nothing is expired.
        let removed = pool.sweep_expired(500);
        assert!(removed.is_empty());
        assert_eq!(pool.len(), 3);

        // At time 1000, tx1 should be removed (expiry <= current).
        let removed = pool.sweep_expired(1000);
        assert_eq!(removed.len(), 1);
        assert_eq!(pool.len(), 2);

        // At time 3000, tx2 should be removed. tx0 stays (expiry == 0).
        let removed = pool.sweep_expired(3000);
        assert_eq!(removed.len(), 1);
        assert_eq!(pool.len(), 1);

        // The remaining tx should have no expiry.
        let remaining = pool.all_transactions();
        assert_eq!(remaining.len(), 1);
    }
}
