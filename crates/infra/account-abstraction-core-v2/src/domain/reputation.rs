use alloy_primitives::Address;
use async_trait::async_trait;

/// Reputation status for an entity
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ReputationStatus {
    /// Entity is not throttled or banned
    Ok,
    /// Entity is throttled
    Throttled,
    /// Entity is banned
    Banned,
}

#[async_trait]
pub trait ReputationService: Send + Sync {
    async fn get_reputation(&self, entity: &Address) -> ReputationStatus;
}
