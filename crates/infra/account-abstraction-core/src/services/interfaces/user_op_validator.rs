use alloy_primitives::Address;
use async_trait::async_trait;

use crate::domain::types::{ValidationResult, VersionedUserOperation};

#[async_trait]
pub trait UserOperationValidator: Send + Sync {
    async fn validate_user_operation(
        &self,
        user_operation: &VersionedUserOperation,
        entry_point: &Address,
    ) -> anyhow::Result<ValidationResult>;
}
