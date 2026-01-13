//! Reputation System
//!
//! Implements ERC-7562 reputation tracking for entities (sender, paymaster, factory, aggregator).
//! The reputation system helps protect the network from entities that consistently fail validation.
//!
//! # Metrics Tracked
//! - `ops_seen`: Number of UserOps seen from this entity
//! - `ops_included`: Number of UserOps successfully included on-chain
//! - `ops_failed`: Number of UserOps that failed 2nd validation (on-chain)
//!
//! # Status Transitions
//! - OK -> Throttled: When inclusion ratio drops below threshold
//! - Throttled -> Banned: When failure ratio exceeds threshold
//! - Banned -> OK: After reputation decay (time-based)

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use alloy_primitives::Address;

use super::config::MempoolConfig;
use super::error::{MempoolError, MempoolResult};

/// Simple counter for probabilistic throttling (1 in 10)
static THROTTLE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Status of an entity's reputation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReputationStatus {
    /// Entity has good reputation, all UserOps accepted
    #[default]
    Ok,

    /// Entity has poor reputation, only 1 in 10 UserOps accepted
    Throttled,

    /// Entity is banned, all UserOps rejected
    Banned,
}

impl ReputationStatus {
    /// Check if UserOps from this entity should be accepted
    pub fn should_accept(&self) -> bool {
        match self {
            Self::Ok => true,
            Self::Throttled => {
                // Probabilistic acceptance: 1 in 10
                // Uses a simple counter-based approach instead of random
                let count = THROTTLE_COUNTER.fetch_add(1, Ordering::Relaxed);
                count % 10 == 0
            }
            Self::Banned => false,
        }
    }
}

/// Reputation data for a single entity
#[derive(Debug, Clone)]
pub struct EntityReputation {
    /// Entity address
    pub address: Address,

    /// Number of UserOps seen from this entity
    pub ops_seen: u64,

    /// Number of UserOps successfully included on-chain
    pub ops_included: u64,

    /// Number of UserOps that failed 2nd validation
    pub ops_failed: u64,

    /// Current reputation status
    pub status: ReputationStatus,

    /// When this entity was first seen
    pub first_seen: Instant,

    /// When this entity was last updated
    pub last_updated: Instant,
}

impl EntityReputation {
    /// Create a new reputation entry for an entity
    pub fn new(address: Address) -> Self {
        let now = Instant::now();
        Self {
            address,
            ops_seen: 0,
            ops_included: 0,
            ops_failed: 0,
            status: ReputationStatus::Ok,
            first_seen: now,
            last_updated: now,
        }
    }

    /// Calculate the inclusion ratio (ops_included / ops_seen)
    pub fn inclusion_ratio(&self) -> f64 {
        if self.ops_seen == 0 {
            1.0 // No data, assume good
        } else {
            self.ops_included as f64 / self.ops_seen as f64
        }
    }

    /// Calculate the failure ratio (ops_failed / ops_seen)
    pub fn failure_ratio(&self) -> f64 {
        if self.ops_seen == 0 {
            0.0 // No data, assume good
        } else {
            self.ops_failed as f64 / self.ops_seen as f64
        }
    }

    /// Update status based on current metrics
    pub fn update_status(&mut self, config: &MempoolConfig) {
        // Don't penalize entities with too few ops
        if self.ops_seen < config.min_ops_for_reputation {
            self.status = ReputationStatus::Ok;
            return;
        }

        let failure_ratio = self.failure_ratio();
        let inclusion_ratio = self.inclusion_ratio();

        if failure_ratio >= config.ban_threshold {
            self.status = ReputationStatus::Banned;
        } else if inclusion_ratio < config.throttle_threshold {
            self.status = ReputationStatus::Throttled;
        } else {
            self.status = ReputationStatus::Ok;
        }
    }

    /// Record that a UserOp was seen
    pub fn record_seen(&mut self) {
        self.ops_seen = self.ops_seen.saturating_add(1);
        self.last_updated = Instant::now();
    }

    /// Record that a UserOp was included on-chain
    pub fn record_included(&mut self) {
        self.ops_included = self.ops_included.saturating_add(1);
        self.last_updated = Instant::now();
    }

    /// Record that a UserOp failed 2nd validation
    pub fn record_failed(&mut self) {
        self.ops_failed = self.ops_failed.saturating_add(1);
        self.last_updated = Instant::now();
    }

    /// Apply decay to counters
    pub fn decay(&mut self, factor: f64) {
        self.ops_seen = (self.ops_seen as f64 * factor) as u64;
        self.ops_included = (self.ops_included as f64 * factor) as u64;
        self.ops_failed = (self.ops_failed as f64 * factor) as u64;
    }
}

/// Manages reputation for all entities
#[derive(Debug)]
pub struct ReputationManager {
    /// Reputation data by entity address
    entities: HashMap<Address, EntityReputation>,

    /// Configuration
    config: MempoolConfig,

    /// When decay was last applied
    last_decay: Instant,
}

impl ReputationManager {
    /// Create a new reputation manager
    pub fn new(config: MempoolConfig) -> Self {
        Self {
            entities: HashMap::new(),
            config,
            last_decay: Instant::now(),
        }
    }

    /// Get or create reputation entry for an entity
    fn get_or_create(&mut self, address: Address) -> &mut EntityReputation {
        self.entities
            .entry(address)
            .or_insert_with(|| EntityReputation::new(address))
    }

    /// Get reputation for an entity (read-only)
    pub fn get(&self, address: &Address) -> Option<&EntityReputation> {
        self.entities.get(address)
    }

    /// Get status for an entity
    pub fn get_status(&self, address: &Address) -> ReputationStatus {
        self.entities
            .get(address)
            .map(|r| r.status)
            .unwrap_or(ReputationStatus::Ok)
    }

    /// Check if all entities in a UserOp have acceptable reputation
    ///
    /// Checks sender, paymaster (if present), factory (if present), and aggregator (if present).
    /// Returns error if any entity is banned or throttled (and random check fails).
    pub fn check_reputation(
        &self,
        sender: Address,
        paymaster: Option<Address>,
        factory: Option<Address>,
        aggregator: Option<Address>,
    ) -> MempoolResult<()> {
        // Check sender
        self.check_entity(sender)?;

        // Check optional entities
        if let Some(pm) = paymaster {
            self.check_entity(pm)?;
        }
        if let Some(f) = factory {
            self.check_entity(f)?;
        }
        if let Some(agg) = aggregator {
            self.check_entity(agg)?;
        }

        Ok(())
    }

    /// Check a single entity's reputation
    fn check_entity(&self, address: Address) -> MempoolResult<()> {
        let status = self.get_status(&address);
        match status {
            ReputationStatus::Ok => Ok(()),
            ReputationStatus::Throttled => {
                if status.should_accept() {
                    Ok(())
                } else {
                    Err(MempoolError::EntityThrottled { entity: address })
                }
            }
            ReputationStatus::Banned => Err(MempoolError::EntityBanned { entity: address }),
        }
    }

    /// Record that UserOps were seen for entities
    pub fn record_seen(
        &mut self,
        sender: Address,
        paymaster: Option<Address>,
        factory: Option<Address>,
        aggregator: Option<Address>,
    ) {
        self.get_or_create(sender).record_seen();

        if let Some(pm) = paymaster {
            self.get_or_create(pm).record_seen();
        }
        if let Some(f) = factory {
            self.get_or_create(f).record_seen();
        }
        if let Some(agg) = aggregator {
            self.get_or_create(agg).record_seen();
        }

        self.maybe_decay();
        self.update_all_statuses();
    }

    /// Record that UserOps were included for entities
    pub fn record_included(
        &mut self,
        sender: Address,
        paymaster: Option<Address>,
        factory: Option<Address>,
        aggregator: Option<Address>,
    ) {
        self.get_or_create(sender).record_included();

        if let Some(pm) = paymaster {
            self.get_or_create(pm).record_included();
        }
        if let Some(f) = factory {
            self.get_or_create(f).record_included();
        }
        if let Some(agg) = aggregator {
            self.get_or_create(agg).record_included();
        }

        self.update_all_statuses();
    }

    /// Record that UserOps failed for entities
    pub fn record_failed(
        &mut self,
        sender: Address,
        paymaster: Option<Address>,
        factory: Option<Address>,
        aggregator: Option<Address>,
    ) {
        self.get_or_create(sender).record_failed();

        if let Some(pm) = paymaster {
            self.get_or_create(pm).record_failed();
        }
        if let Some(f) = factory {
            self.get_or_create(f).record_failed();
        }
        if let Some(agg) = aggregator {
            self.get_or_create(agg).record_failed();
        }

        self.update_all_statuses();
    }

    /// Apply decay if enough time has passed
    fn maybe_decay(&mut self) {
        let elapsed = self.last_decay.elapsed();
        if elapsed >= self.config.reputation_decay_interval {
            let factor = self.config.reputation_decay_factor;
            for rep in self.entities.values_mut() {
                rep.decay(factor);
            }
            self.last_decay = Instant::now();
        }
    }

    /// Update status for all entities
    fn update_all_statuses(&mut self) {
        let config = self.config.clone();
        for rep in self.entities.values_mut() {
            rep.update_status(&config);
        }
    }

    /// Clear all reputation data (for testing)
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.entities.clear();
    }

    /// Get total number of tracked entities
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_address(n: u8) -> Address {
        Address::new([n; 20])
    }

    #[test]
    fn test_new_entity_is_ok() {
        let manager = ReputationManager::new(MempoolConfig::default());
        let status = manager.get_status(&test_address(1));
        assert_eq!(status, ReputationStatus::Ok);
    }

    #[test]
    fn test_reputation_check_passes_for_new_entity() {
        let manager = ReputationManager::new(MempoolConfig::default());
        let result = manager.check_reputation(test_address(1), None, None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_record_seen() {
        let mut manager = ReputationManager::new(MempoolConfig::default());
        manager.record_seen(test_address(1), None, None, None);

        let rep = manager.get(&test_address(1)).unwrap();
        assert_eq!(rep.ops_seen, 1);
        assert_eq!(rep.ops_included, 0);
    }

    #[test]
    fn test_record_included() {
        let mut manager = ReputationManager::new(MempoolConfig::default());
        manager.record_seen(test_address(1), None, None, None);
        manager.record_included(test_address(1), None, None, None);

        let rep = manager.get(&test_address(1)).unwrap();
        assert_eq!(rep.ops_seen, 1);
        assert_eq!(rep.ops_included, 1);
    }

    #[test]
    fn test_banning_after_failures() {
        let config = MempoolConfig {
            min_ops_for_reputation: 2,
            ban_threshold: 0.5,
            ..Default::default()
        };
        let mut manager = ReputationManager::new(config);

        let addr = test_address(1);

        // Record enough ops to trigger reputation check
        for _ in 0..4 {
            manager.record_seen(addr, None, None, None);
        }
        // Fail most of them
        for _ in 0..3 {
            manager.record_failed(addr, None, None, None);
        }

        let status = manager.get_status(&addr);
        assert_eq!(status, ReputationStatus::Banned);
    }

    #[test]
    fn test_inclusion_ratio() {
        let mut rep = EntityReputation::new(test_address(1));
        assert_eq!(rep.inclusion_ratio(), 1.0); // No ops, assume good

        rep.ops_seen = 10;
        rep.ops_included = 5;
        assert_eq!(rep.inclusion_ratio(), 0.5);
    }

    #[test]
    fn test_failure_ratio() {
        let mut rep = EntityReputation::new(test_address(1));
        assert_eq!(rep.failure_ratio(), 0.0); // No ops, assume good

        rep.ops_seen = 10;
        rep.ops_failed = 2;
        assert_eq!(rep.failure_ratio(), 0.2);
    }

    #[test]
    fn test_decay() {
        let mut rep = EntityReputation::new(test_address(1));
        rep.ops_seen = 100;
        rep.ops_included = 50;
        rep.ops_failed = 10;

        rep.decay(0.5);

        assert_eq!(rep.ops_seen, 50);
        assert_eq!(rep.ops_included, 25);
        assert_eq!(rep.ops_failed, 5);
    }
}
