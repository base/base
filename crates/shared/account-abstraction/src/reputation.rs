//! Reputation tracking for entities in ERC-4337.

use alloy_primitives::Address;
use async_trait::async_trait;

/// Reputation status for an entity.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ReputationStatus {
    /// Entity is not throttled or banned.
    Ok,
    /// Entity is throttled.
    Throttled,
    /// Entity is banned.
    Banned,
}

/// A trait for querying the reputation status of entities.
#[async_trait]
pub trait ReputationService: Send + Sync {
    /// Returns the reputation status of the given entity address.
    async fn get_reputation(&self, entity: &Address) -> ReputationStatus;
}
