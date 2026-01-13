//! UserOperation Pool
//!
//! Core mempool data structures for storing and managing UserOperations.
//!
//! # Architecture
//!
//! - `UserOpPool`: Top-level container with per-entrypoint pools
//! - `EntryPointPool`: Stores UserOps for a single entrypoint
//! - `PooledUserOp`: A UserOp with metadata for pool management

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use alloy_primitives::{Address, B256, U256};
use parking_lot::RwLock;
use tracing::{debug, info};

use crate::rpc::UserOperation;
use crate::simulation::{EntityCodeHashes, ValidationOutput};

use super::config::MempoolConfig;
use super::error::{MempoolError, MempoolResult};
use super::reputation::ReputationManager;
use super::rules::MempoolRuleChecker;

/// Status of a UserOp in the pool
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserOpStatus {
    /// Ready to be included in a bundle
    Ready,

    /// Marked for inclusion by the builder (not returned by get_best_userops)
    PendingInclusion {
        /// When the builder marked this for inclusion
        marked_at: Instant,
    },
}

impl Default for UserOpStatus {
    fn default() -> Self {
        Self::Ready
    }
}

/// A UserOperation stored in the mempool with metadata
#[derive(Debug, Clone)]
pub struct PooledUserOp {
    /// The UserOperation
    pub user_op: UserOperation,

    /// UserOperation hash
    pub hash: B256,

    /// EntryPoint address this UserOp is for
    pub entry_point: Address,

    /// Sender address (cached for quick lookup)
    pub sender: Address,

    /// Nonce (cached for ordering)
    pub nonce: U256,

    /// Paymaster address (if any)
    pub paymaster: Option<Address>,

    /// Factory address (if any)
    pub factory: Option<Address>,

    /// Aggregator address (if any)
    pub aggregator: Option<Address>,

    /// Effective gas price for priority ordering
    /// Calculated as: min(maxFeePerGas, maxPriorityFeePerGas + baseFee)
    pub effective_gas_price: U256,

    /// Max fee per gas from the UserOp
    pub max_fee_per_gas: U256,

    /// Max priority fee per gas from the UserOp
    pub max_priority_fee_per_gas: U256,

    /// Validation output from initial validation
    pub validation_output: ValidationOutput,

    /// Code hashes at validation time (for COD-010)
    pub code_hashes: Option<EntityCodeHashes>,

    /// Storage slots accessed during validation (for STO-041)
    pub storage_slots: HashSet<(Address, B256)>,

    /// When this UserOp was added to the pool
    pub added_at: Instant,

    /// When this UserOp expires (from validUntil or max TTL)
    pub expires_at: Instant,

    /// Current status in the pool
    pub status: UserOpStatus,
}

impl PooledUserOp {
    /// Create a new pooled UserOp
    pub fn new(
        user_op: UserOperation,
        hash: B256,
        entry_point: Address,
        validation_output: ValidationOutput,
        config: &MempoolConfig,
    ) -> Self {
        let now = Instant::now();

        // Calculate expiry: min of (validUntil, now + max_ttl)
        let valid_until = validation_output.return_info.valid_until;
        let max_expiry = now + config.max_time_in_pool;

        let expires_at = if valid_until == 0 {
            // validUntil=0 means no expiry from the UserOp, use max TTL
            max_expiry
        } else {
            // Calculate when validUntil timestamp would be reached
            let current_timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();

            if valid_until <= current_timestamp {
                // Already expired
                now
            } else {
                let time_until_expiry =
                    std::time::Duration::from_secs(valid_until - current_timestamp);
                let valid_until_instant = now + time_until_expiry;
                // Take the earlier of validUntil and max TTL
                std::cmp::min(valid_until_instant, max_expiry)
            }
        };

        // Extract fields from UserOp
        let (sender, nonce, max_fee_per_gas, max_priority_fee_per_gas) = match &user_op {
            UserOperation::V06(op) => (op.sender, op.nonce, op.max_fee_per_gas, op.max_priority_fee_per_gas),
            UserOperation::V07(op) => (op.sender, op.nonce, op.max_fee_per_gas, op.max_priority_fee_per_gas),
        };

        let paymaster = user_op.paymaster();
        let factory = user_op.factory();

        // Get aggregator from validation output
        let aggregator = validation_output
            .aggregator_info
            .as_ref()
            .map(|a| a.aggregator);

        // For now, use max_fee_per_gas as effective gas price
        // TODO: Calculate properly with current base fee
        let effective_gas_price = max_fee_per_gas;

        // Extract code hashes
        let code_hashes = validation_output.code_hashes.clone();

        Self {
            user_op,
            hash,
            entry_point,
            sender,
            nonce,
            paymaster,
            factory,
            aggregator,
            effective_gas_price,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            validation_output,
            code_hashes,
            storage_slots: HashSet::new(), // TODO: Extract from validation trace
            added_at: now,
            expires_at,
            status: UserOpStatus::Ready,
        }
    }

    /// Check if this UserOp has expired
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    /// Check if this UserOp is ready (not pending inclusion)
    pub fn is_ready(&self) -> bool {
        matches!(self.status, UserOpStatus::Ready)
    }

    /// Get time remaining until expiry
    pub fn time_to_expiry(&self) -> std::time::Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    /// Check if a replacement UserOp has sufficient gas price bump
    pub fn can_be_replaced_by(&self, replacement: &PooledUserOp, config: &MempoolConfig) -> bool {
        let required =
            config.required_replacement_gas_price(self.effective_gas_price.to::<u128>());
        replacement.effective_gas_price.to::<u128>() >= required
    }
}

/// Pool for a single entrypoint
#[derive(Debug)]
pub struct EntryPointPool {
    /// Entrypoint address
    pub entry_point: Address,

    /// UserOps indexed by hash
    by_hash: HashMap<B256, PooledUserOp>,

    /// UserOps indexed by sender -> (nonce -> hash)
    /// BTreeMap for nonce ordering
    by_sender: HashMap<Address, BTreeMap<U256, B256>>,

    /// UserOps indexed by paymaster (for limit tracking)
    by_paymaster: HashMap<Address, HashSet<B256>>,

    /// UserOps indexed by factory (for limit tracking)
    by_factory: HashMap<Address, HashSet<B256>>,

    /// Storage slots accessed by UserOps in this pool
    /// Maps (contract_address, slot) -> set of UserOp hashes
    /// Used for STO-041 conflict detection
    storage_slots: HashMap<(Address, B256), HashSet<B256>>,

    /// Sender addresses used by UserOps (for STO-040)
    /// Maps sender -> UserOp hash
    sender_addresses: HashMap<Address, B256>,

    /// Configuration
    config: MempoolConfig,
}

impl EntryPointPool {
    /// Create a new pool for an entrypoint
    pub fn new(entry_point: Address, config: MempoolConfig) -> Self {
        Self {
            entry_point,
            by_hash: HashMap::new(),
            by_sender: HashMap::new(),
            by_paymaster: HashMap::new(),
            by_factory: HashMap::new(),
            storage_slots: HashMap::new(),
            sender_addresses: HashMap::new(),
            config,
        }
    }

    /// Get the number of UserOps in the pool
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// Check if the pool is empty
    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    /// Get a UserOp by hash
    pub fn get(&self, hash: &B256) -> Option<&PooledUserOp> {
        self.by_hash.get(hash)
    }

    /// Get a mutable reference to a UserOp by hash
    pub fn get_mut(&mut self, hash: &B256) -> Option<&mut PooledUserOp> {
        self.by_hash.get_mut(hash)
    }

    /// Check if a UserOp exists
    pub fn contains(&self, hash: &B256) -> bool {
        self.by_hash.contains_key(hash)
    }

    /// Get UserOps for a sender
    pub fn get_by_sender(&self, sender: &Address) -> Vec<&PooledUserOp> {
        self.by_sender
            .get(sender)
            .map(|nonces| {
                nonces
                    .values()
                    .filter_map(|hash| self.by_hash.get(hash))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Count UserOps for a sender
    pub fn count_by_sender(&self, sender: &Address) -> usize {
        self.by_sender.get(sender).map(|m| m.len()).unwrap_or(0)
    }

    /// Count UserOps for a paymaster
    pub fn count_by_paymaster(&self, paymaster: &Address) -> usize {
        self.by_paymaster
            .get(paymaster)
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// Count UserOps for a factory
    pub fn count_by_factory(&self, factory: &Address) -> usize {
        self.by_factory.get(factory).map(|s| s.len()).unwrap_or(0)
    }

    /// Get existing UserOp with same sender and nonce (for replacement)
    pub fn get_existing_for_replacement(
        &self,
        sender: &Address,
        nonce: &U256,
    ) -> Option<&PooledUserOp> {
        self.by_sender
            .get(sender)
            .and_then(|nonces| nonces.get(nonce))
            .and_then(|hash| self.by_hash.get(hash))
    }

    /// Add a UserOp to the pool
    ///
    /// This does NOT check limits - caller should check first.
    pub fn add(&mut self, user_op: PooledUserOp) -> MempoolResult<()> {
        let hash = user_op.hash;
        let sender = user_op.sender;
        let nonce = user_op.nonce;

        // Check for replacement (same sender, same nonce) BEFORE checking hash
        // This handles the case where the UserOp content is identical (same hash)
        // but we want to return ReplacementGasPriceTooLow instead of AlreadyExists
        if let Some(existing_hash) = self.by_sender.get(&sender).and_then(|m| m.get(&nonce)) {
            let existing = self.by_hash.get(existing_hash).unwrap();

            // If hashes are the same, it's the exact same UserOp - treat as replacement with insufficient bump
            if *existing_hash == hash {
                let required = self
                    .config
                    .required_replacement_gas_price(existing.effective_gas_price.to::<u128>());
                return Err(MempoolError::replacement_gas_too_low(
                    existing.effective_gas_price,
                    user_op.effective_gas_price,
                    U256::from(required),
                ));
            }

            // Check if gas price is high enough for replacement
            if !existing.can_be_replaced_by(&user_op, &self.config) {
                let required = self
                    .config
                    .required_replacement_gas_price(existing.effective_gas_price.to::<u128>());
                return Err(MempoolError::replacement_gas_too_low(
                    existing.effective_gas_price,
                    user_op.effective_gas_price,
                    U256::from(required),
                ));
            }

            // Remove the existing one
            let existing_hash = *existing_hash;
            self.remove_internal(&existing_hash);

            debug!(
                target: "aa-mempool",
                old_hash = %existing_hash,
                new_hash = %hash,
                sender = %sender,
                nonce = %nonce,
                "Replaced UserOp with higher gas price"
            );
        } else if self.by_hash.contains_key(&hash) {
            // Different sender/nonce but same hash - this shouldn't happen normally
            // but handle it just in case
            return Err(MempoolError::AlreadyExists(hash));
        }

        // Index by sender
        self.by_sender
            .entry(sender)
            .or_default()
            .insert(nonce, hash);

        // Index by paymaster
        if let Some(pm) = user_op.paymaster {
            self.by_paymaster.entry(pm).or_default().insert(hash);
        }

        // Index by factory
        if let Some(f) = user_op.factory {
            self.by_factory.entry(f).or_default().insert(hash);
        }

        // Track storage slots
        for slot in &user_op.storage_slots {
            self.storage_slots.entry(*slot).or_default().insert(hash);
        }

        // Track sender address (for STO-040)
        self.sender_addresses.insert(sender, hash);

        // Add to main index
        self.by_hash.insert(hash, user_op);

        Ok(())
    }

    /// Remove a UserOp from the pool
    pub fn remove(&mut self, hash: &B256) -> Option<PooledUserOp> {
        self.remove_internal(hash)
    }

    /// Internal remove implementation
    fn remove_internal(&mut self, hash: &B256) -> Option<PooledUserOp> {
        let user_op = self.by_hash.remove(hash)?;

        // Remove from sender index
        if let Some(nonces) = self.by_sender.get_mut(&user_op.sender) {
            nonces.remove(&user_op.nonce);
            if nonces.is_empty() {
                self.by_sender.remove(&user_op.sender);
            }
        }

        // Remove from paymaster index
        if let Some(pm) = user_op.paymaster {
            if let Some(hashes) = self.by_paymaster.get_mut(&pm) {
                hashes.remove(hash);
                if hashes.is_empty() {
                    self.by_paymaster.remove(&pm);
                }
            }
        }

        // Remove from factory index
        if let Some(f) = user_op.factory {
            if let Some(hashes) = self.by_factory.get_mut(&f) {
                hashes.remove(hash);
                if hashes.is_empty() {
                    self.by_factory.remove(&f);
                }
            }
        }

        // Remove from storage slot index
        for slot in &user_op.storage_slots {
            if let Some(hashes) = self.storage_slots.get_mut(slot) {
                hashes.remove(hash);
                if hashes.is_empty() {
                    self.storage_slots.remove(slot);
                }
            }
        }

        // Remove from sender addresses
        if self.sender_addresses.get(&user_op.sender) == Some(hash) {
            self.sender_addresses.remove(&user_op.sender);
        }

        Some(user_op)
    }

    /// Remove expired UserOps and return their hashes
    pub fn remove_expired(&mut self) -> Vec<B256> {
        let expired: Vec<B256> = self
            .by_hash
            .iter()
            .filter(|(_, op)| op.is_expired())
            .map(|(hash, _)| *hash)
            .collect();

        for hash in &expired {
            self.remove_internal(hash);
        }

        expired
    }

    /// Get best UserOps for bundling, ordered by effective gas price
    ///
    /// Returns UserOps that are:
    /// - Status == Ready (not pending inclusion)
    /// - Not expired
    /// - Respecting nonce ordering per sender
    pub fn get_best_userops(&self, max_count: usize, max_gas: u64) -> Vec<&PooledUserOp> {
        // Collect all ready, non-expired UserOps
        let mut candidates: Vec<&PooledUserOp> = self
            .by_hash
            .values()
            .filter(|op| op.is_ready() && !op.is_expired())
            .collect();

        // Sort by effective gas price (descending)
        candidates.sort_by(|a, b| b.effective_gas_price.cmp(&a.effective_gas_price));

        // Select up to max_count, respecting gas limit
        // TODO: Proper gas accounting and nonce ordering
        let mut selected = Vec::new();
        let mut _total_gas = 0u64;

        for op in candidates {
            if selected.len() >= max_count {
                break;
            }

            // TODO: Calculate actual gas for this UserOp
            let op_gas = 100_000u64; // Placeholder
            if _total_gas + op_gas > max_gas {
                continue;
            }

            selected.push(op);
            _total_gas += op_gas;
        }

        selected
    }

    /// Get the minimum gas price in the pool (for eviction)
    pub fn min_gas_price(&self) -> Option<U256> {
        self.by_hash.values().map(|op| op.effective_gas_price).min()
    }

    /// Evict lowest gas price UserOps until we have room
    ///
    /// Returns the evicted UserOps
    pub fn evict_to_make_room(&mut self, count: usize) -> Vec<PooledUserOp> {
        if count == 0 || self.by_hash.is_empty() {
            return Vec::new();
        }

        // Find the lowest gas price UserOps
        let mut by_price: Vec<(B256, U256)> = self
            .by_hash
            .iter()
            .map(|(hash, op)| (*hash, op.effective_gas_price))
            .collect();

        // Sort by gas price ascending
        by_price.sort_by(|a, b| a.1.cmp(&b.1));

        // Evict the lowest ones
        let to_evict: Vec<B256> = by_price.into_iter().take(count).map(|(h, _)| h).collect();

        let mut evicted = Vec::new();
        for hash in to_evict {
            if let Some(op) = self.remove_internal(&hash) {
                debug!(
                    target: "aa-mempool",
                    hash = %hash,
                    sender = %op.sender,
                    gas_price = %op.effective_gas_price,
                    "Evicted low gas price UserOp"
                );
                evicted.push(op);
            }
        }

        evicted
    }

    /// Get all UserOp hashes in the pool
    pub fn all_hashes(&self) -> Vec<B256> {
        self.by_hash.keys().copied().collect()
    }

    /// Check for storage slot conflicts (STO-041)
    pub fn has_storage_conflict(&self, slots: &HashSet<(Address, B256)>) -> Option<B256> {
        for slot in slots {
            if let Some(hashes) = self.storage_slots.get(slot) {
                if let Some(hash) = hashes.iter().next() {
                    return Some(*hash);
                }
            }
        }
        None
    }

    /// Check if an address is used as a sender (STO-040)
    pub fn is_sender_address(&self, address: &Address) -> bool {
        self.sender_addresses.contains_key(address)
    }
}

/// Top-level UserOperation pool
pub struct UserOpPool {
    /// Per-entrypoint pools
    pools: HashMap<Address, EntryPointPool>,

    /// Reputation manager
    pub reputation: ReputationManager,

    /// Mempool rule checker
    rule_checker: MempoolRuleChecker,

    /// Configuration
    config: MempoolConfig,

    /// Chain ID (for hash calculation)
    chain_id: u64,
}

impl UserOpPool {
    /// Create a new UserOp pool
    pub fn new(config: MempoolConfig, chain_id: u64) -> Self {
        Self {
            pools: HashMap::new(),
            reputation: ReputationManager::new(config.clone()),
            rule_checker: MempoolRuleChecker::new(),
            config,
            chain_id,
        }
    }

    /// Create a new UserOp pool wrapped in Arc<RwLock>
    pub fn new_shared(config: MempoolConfig, chain_id: u64) -> Arc<RwLock<Self>> {
        Arc::new(RwLock::new(Self::new(config, chain_id)))
    }

    /// Get a pool for an entrypoint (read-only)
    pub fn get_pool(&self, entry_point: &Address) -> Option<&EntryPointPool> {
        self.pools.get(entry_point)
    }

    /// Add a validated UserOp to the pool
    ///
    /// The UserOp should already be validated. This method:
    /// 1. Checks reputation
    /// 2. Checks mempool rules (STO-040, STO-041, AUTH-040)
    /// 3. Checks limits (sender, paymaster, factory)
    /// 4. Adds to pool (with replacement if needed)
    /// 5. Evicts if pool is full
    pub fn add(
        &mut self,
        user_op: UserOperation,
        entry_point: Address,
        validation_output: ValidationOutput,
    ) -> MempoolResult<B256> {
        // Calculate hash
        let hash = user_op.hash(entry_point, self.chain_id);

        // Clone config values we need to avoid borrow issues
        let max_ops_per_sender = self.config.max_ops_per_sender;
        let max_ops_per_unstaked_paymaster = self.config.max_ops_per_unstaked_paymaster;
        let max_ops_per_unstaked_factory = self.config.max_ops_per_unstaked_factory;
        let max_pool_size = self.config.max_pool_size_per_entrypoint;

        // Create pooled UserOp
        let pooled = PooledUserOp::new(
            user_op,
            hash,
            entry_point,
            validation_output,
            &self.config,
        );

        // Check expiry
        if pooled.is_expired() {
            return Err(MempoolError::Expired {
                valid_until: pooled.validation_output.return_info.valid_until,
            });
        }

        // Check reputation
        self.reputation.check_reputation(
            pooled.sender,
            pooled.paymaster,
            pooled.factory,
            pooled.aggregator,
        )?;

        // Record reputation (do this before getting pool to avoid borrow issues)
        self.reputation.record_seen(
            pooled.sender,
            pooled.paymaster,
            pooled.factory,
            pooled.aggregator,
        );

        // Get or create pool - need to ensure pool exists first for rule checking
        // Then do all checks and finally add
        self.pools
            .entry(entry_point)
            .or_insert_with(|| EntryPointPool::new(entry_point, self.config.clone()));

        // Now borrow the pool immutably for checks
        {
            let pool = self.pools.get(&entry_point).unwrap();

            // Check mempool rules
            self.rule_checker.check(&pooled, pool)?;

            // Check sender limit (unless this is a replacement)
            let existing = pool.get_existing_for_replacement(&pooled.sender, &pooled.nonce);
            if existing.is_none() {
                let sender_count = pool.count_by_sender(&pooled.sender);
                if sender_count >= max_ops_per_sender {
                    return Err(MempoolError::sender_limit(pooled.sender, max_ops_per_sender));
                }
            }

            // Check paymaster limit (if unstaked)
            if let Some(pm) = pooled.paymaster {
                let pm_info = pooled.validation_output.paymaster_info.as_ref();
                let is_staked = pm_info.map(|i| i.is_staked).unwrap_or(false);

                if !is_staked {
                    let pm_count = pool.count_by_paymaster(&pm);
                    if pm_count >= max_ops_per_unstaked_paymaster {
                        return Err(MempoolError::paymaster_limit(
                            pm,
                            max_ops_per_unstaked_paymaster,
                        ));
                    }
                }
            }

            // Check factory limit (if unstaked)
            if let Some(f) = pooled.factory {
                let f_info = pooled.validation_output.factory_info.as_ref();
                let is_staked = f_info.map(|i| i.is_staked).unwrap_or(false);

                if !is_staked {
                    let f_count = pool.count_by_factory(&f);
                    if f_count >= max_ops_per_unstaked_factory {
                        return Err(MempoolError::factory_limit(f, max_ops_per_unstaked_factory));
                    }
                }
            }

            // Check pool size limit
            if pool.len() >= max_pool_size {
                let min_price = pool.min_gas_price().unwrap_or(U256::MAX);
                if pooled.effective_gas_price <= min_price {
                    return Err(MempoolError::MempoolFull {
                        gas_price: pooled.effective_gas_price,
                        min_gas_price: min_price,
                    });
                }
            }
        }

        // Now get mutable reference and add
        let pool = self.pools.get_mut(&entry_point).unwrap();

        // Evict if needed
        if pool.len() >= max_pool_size {
            pool.evict_to_make_room(1);
        }

        // Add to pool
        pool.add(pooled)?;

        info!(
            target: "aa-mempool",
            hash = %hash,
            entry_point = %entry_point,
            pool_size = pool.len(),
            "Added UserOp to mempool"
        );

        Ok(hash)
    }

    /// Remove a UserOp from the pool
    pub fn remove(&mut self, entry_point: &Address, hash: &B256) -> Option<PooledUserOp> {
        self.pools.get_mut(entry_point).and_then(|p| p.remove(hash))
    }

    /// Get a UserOp by hash
    pub fn get(&self, entry_point: &Address, hash: &B256) -> Option<&PooledUserOp> {
        self.pools.get(entry_point).and_then(|p| p.get(hash))
    }

    /// Check if a UserOp exists
    pub fn contains(&self, entry_point: &Address, hash: &B256) -> bool {
        self.pools
            .get(entry_point)
            .map(|p| p.contains(hash))
            .unwrap_or(false)
    }

    /// Mark UserOps as pending inclusion
    ///
    /// Called by the builder when starting to build a bundle.
    /// These UserOps won't be returned by get_best_userops until released.
    pub fn mark_pending_inclusion(&mut self, entry_point: &Address, hashes: &[B256]) {
        if let Some(pool) = self.pools.get_mut(entry_point) {
            for hash in hashes {
                if let Some(op) = pool.get_mut(hash) {
                    op.status = UserOpStatus::PendingInclusion {
                        marked_at: Instant::now(),
                    };
                }
            }
        }
    }

    /// Confirm UserOps were included
    ///
    /// Called by the builder after successful bundle submission.
    /// Removes UserOps from the pool and updates reputation.
    pub fn confirm_included(
        &mut self,
        entry_point: &Address,
        hashes: &[B256],
    ) -> Vec<PooledUserOp> {
        let mut removed = Vec::new();

        if let Some(pool) = self.pools.get_mut(entry_point) {
            for hash in hashes {
                if let Some(op) = pool.remove(hash) {
                    // Update reputation
                    self.reputation.record_included(
                        op.sender,
                        op.paymaster,
                        op.factory,
                        op.aggregator,
                    );
                    removed.push(op);
                }
            }
        }

        if !removed.is_empty() {
            info!(
                target: "aa-mempool",
                entry_point = %entry_point,
                count = removed.len(),
                "Confirmed UserOps included"
            );
        }

        removed
    }

    /// Release UserOps back to ready state
    ///
    /// Called by the builder if bundle building fails.
    pub fn release_pending(&mut self, entry_point: &Address, hashes: &[B256]) {
        if let Some(pool) = self.pools.get_mut(entry_point) {
            for hash in hashes {
                if let Some(op) = pool.get_mut(hash) {
                    op.status = UserOpStatus::Ready;
                }
            }
        }
    }

    /// Remove UserOps that were included on-chain
    ///
    /// Called by the block watcher when a block is committed.
    /// Returns the removed UserOps.
    pub fn remove_included(
        &mut self,
        entry_point: &Address,
        hashes: &[B256],
    ) -> Vec<PooledUserOp> {
        // Use confirm_included since the behavior is the same
        self.confirm_included(entry_point, hashes)
    }

    /// Mark UserOps as failed (for reputation)
    ///
    /// Called when a UserOp fails 2nd validation on-chain.
    pub fn record_failed(&mut self, entry_point: &Address, hashes: &[B256]) {
        if let Some(pool) = self.pools.get(entry_point) {
            for hash in hashes {
                if let Some(op) = pool.get(hash) {
                    self.reputation.record_failed(
                        op.sender,
                        op.paymaster,
                        op.factory,
                        op.aggregator,
                    );
                }
            }
        }
    }

    /// Get best UserOps for bundling
    pub fn get_best_userops(
        &self,
        entry_point: &Address,
        max_count: usize,
        max_gas: u64,
    ) -> Vec<&PooledUserOp> {
        self.pools
            .get(entry_point)
            .map(|p| p.get_best_userops(max_count, max_gas))
            .unwrap_or_default()
    }

    /// Remove expired UserOps from all pools
    pub fn remove_expired(&mut self) -> Vec<(Address, B256)> {
        let mut removed = Vec::new();

        for (entry_point, pool) in self.pools.iter_mut() {
            let expired = pool.remove_expired();
            for hash in expired {
                removed.push((*entry_point, hash));
            }
        }

        if !removed.is_empty() {
            debug!(
                target: "aa-mempool",
                count = removed.len(),
                "Removed expired UserOps"
            );
        }

        removed
    }

    /// Get total number of UserOps across all pools
    pub fn total_count(&self) -> usize {
        self.pools.values().map(|p| p.len()).sum()
    }

    /// Get count for a specific entrypoint
    pub fn count(&self, entry_point: &Address) -> usize {
        self.pools.get(entry_point).map(|p| p.len()).unwrap_or(0)
    }

    /// Get all entrypoints with pools
    pub fn entrypoints(&self) -> Vec<Address> {
        self.pools.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::UserOperationV07;
    use crate::simulation::{ReturnInfo, StakeInfo, ValidationOutput};
    use alloy_primitives::Bytes;

    fn test_address(n: u8) -> Address {
        Address::new([n; 20])
    }

    fn test_user_op(sender: Address, nonce: u64, max_fee: u128) -> UserOperation {
        UserOperation::V07(UserOperationV07 {
            sender,
            nonce: U256::from(nonce),
            factory: Address::ZERO,
            factory_data: Bytes::default(),
            call_data: Bytes::default(),
            call_gas_limit: U256::from(100_000),
            verification_gas_limit: U256::from(100_000),
            pre_verification_gas: U256::from(50_000),
            max_fee_per_gas: U256::from(max_fee),
            max_priority_fee_per_gas: U256::from(1_000_000_000),
            paymaster: Address::ZERO,
            paymaster_verification_gas_limit: U256::ZERO,
            paymaster_post_op_gas_limit: U256::ZERO,
            paymaster_data: Bytes::default(),
            signature: Bytes::default(),
        })
    }

    fn test_validation_output(sender: Address) -> ValidationOutput {
        ValidationOutput {
            valid: true,
            return_info: ReturnInfo {
                pre_op_gas: 100_000,
                prefund: U256::from(1_000_000_000_000_000u64),
                sig_failed: false,
                valid_after: 0,
                valid_until: 0, // No expiry
                paymaster_context: Bytes::default(),
            },
            sender_info: StakeInfo {
                address: sender,
                stake: U256::ZERO,
                unstake_delay_sec: 0,
                deposit: U256::from(1_000_000_000_000_000u64),
                is_staked: false,
            },
            factory_info: None,
            paymaster_info: None,
            aggregator_info: None,
            code_hashes: None,
            violations: vec![],
            trace: None,
        }
    }

    #[test]
    fn test_add_userops() {
        let config = MempoolConfig::default();
        let mut pool = UserOpPool::new(config, 1);

        let entry_point = test_address(1);
        let sender = test_address(2);
        let user_op = test_user_op(sender, 0, 10_000_000_000);
        let validation = test_validation_output(sender);

        let result = pool.add(user_op, entry_point, validation);
        assert!(result.is_ok());
        assert_eq!(pool.total_count(), 1);
    }

    #[test]
    fn test_sender_limit() {
        let config = MempoolConfig {
            max_ops_per_sender: 2,
            ..Default::default()
        };
        let mut pool = UserOpPool::new(config, 1);

        let entry_point = test_address(1);
        let sender = test_address(2);

        // Add 2 UserOps (at limit)
        for nonce in 0..2 {
            let user_op = test_user_op(sender, nonce, 10_000_000_000);
            let validation = test_validation_output(sender);
            pool.add(user_op, entry_point, validation).unwrap();
        }

        // Third should fail
        let user_op = test_user_op(sender, 2, 10_000_000_000);
        let validation = test_validation_output(sender);
        let result = pool.add(user_op, entry_point, validation);

        assert!(matches!(result, Err(MempoolError::SenderLimitReached { .. })));
    }

    #[test]
    fn test_replacement() {
        let config = MempoolConfig {
            replacement_gas_bump_percent: 10,
            ..Default::default()
        };
        let mut pool = UserOpPool::new(config, 1);

        let entry_point = test_address(1);
        let sender = test_address(2);

        // Add initial UserOp
        let user_op1 = test_user_op(sender, 0, 10_000_000_000);
        let validation1 = test_validation_output(sender);
        let hash1 = pool.add(user_op1, entry_point, validation1).unwrap();

        // Try to replace with same gas price (should fail)
        let user_op2 = test_user_op(sender, 0, 10_000_000_000);
        let validation2 = test_validation_output(sender);
        let result = pool.add(user_op2, entry_point, validation2);
        assert!(matches!(
            result,
            Err(MempoolError::ReplacementGasPriceTooLow { .. })
        ));

        // Replace with higher gas price (should succeed)
        let user_op3 = test_user_op(sender, 0, 12_000_000_000); // 20% bump
        let validation3 = test_validation_output(sender);
        let hash3 = pool.add(user_op3, entry_point, validation3).unwrap();

        // Pool should still have 1 UserOp (replaced)
        assert_eq!(pool.total_count(), 1);
        assert!(!pool.contains(&entry_point, &hash1));
        assert!(pool.contains(&entry_point, &hash3));
    }

    #[test]
    fn test_get_best_userops() {
        let config = MempoolConfig::default();
        let mut pool = UserOpPool::new(config, 1);

        let entry_point = test_address(1);

        // Add UserOps with different gas prices
        for (i, gas_price) in [5, 10, 3, 8, 15].iter().enumerate() {
            let sender = test_address((i + 10) as u8);
            let user_op = test_user_op(sender, 0, *gas_price * 1_000_000_000);
            let validation = test_validation_output(sender);
            pool.add(user_op, entry_point, validation).unwrap();
        }

        // Get best 3
        let best = pool.get_best_userops(&entry_point, 3, u64::MAX);
        assert_eq!(best.len(), 3);

        // Should be ordered by gas price descending
        assert!(best[0].effective_gas_price >= best[1].effective_gas_price);
        assert!(best[1].effective_gas_price >= best[2].effective_gas_price);
    }

    #[test]
    fn test_mark_pending_and_release() {
        let config = MempoolConfig::default();
        let mut pool = UserOpPool::new(config, 1);

        let entry_point = test_address(1);
        let sender = test_address(2);
        let user_op = test_user_op(sender, 0, 10_000_000_000);
        let validation = test_validation_output(sender);
        let hash = pool.add(user_op, entry_point, validation).unwrap();

        // Initially should be in best
        assert_eq!(pool.get_best_userops(&entry_point, 10, u64::MAX).len(), 1);

        // Mark pending
        pool.mark_pending_inclusion(&entry_point, &[hash]);

        // Should not be in best anymore
        assert_eq!(pool.get_best_userops(&entry_point, 10, u64::MAX).len(), 0);

        // Release
        pool.release_pending(&entry_point, &[hash]);

        // Should be back in best
        assert_eq!(pool.get_best_userops(&entry_point, 10, u64::MAX).len(), 1);
    }

    #[test]
    fn test_confirm_included() {
        let config = MempoolConfig::default();
        let mut pool = UserOpPool::new(config, 1);

        let entry_point = test_address(1);
        let sender = test_address(2);
        let user_op = test_user_op(sender, 0, 10_000_000_000);
        let validation = test_validation_output(sender);
        let hash = pool.add(user_op, entry_point, validation).unwrap();

        assert_eq!(pool.total_count(), 1);

        // Confirm included
        let removed = pool.confirm_included(&entry_point, &[hash]);
        assert_eq!(removed.len(), 1);
        assert_eq!(pool.total_count(), 0);
    }
}
