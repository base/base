//! Mempool Error Types
//!
//! Error types for mempool operations.

use alloy_primitives::{Address, B256, U256};
use thiserror::Error;

use crate::simulation::Erc7562Rule;

/// Result type for mempool operations
pub type MempoolResult<T> = Result<T, MempoolError>;

/// Errors that can occur during mempool operations
#[derive(Debug, Error)]
pub enum MempoolError {
    /// UserOp already exists in the mempool
    #[error("UserOp {0} already exists in mempool")]
    AlreadyExists(B256),

    /// Sender has too many UserOps in the mempool
    #[error("Sender {sender} has reached maximum UserOps ({max})")]
    SenderLimitReached { sender: Address, max: usize },

    /// Unstaked paymaster has too many UserOps
    #[error("Unstaked paymaster {paymaster} has reached maximum UserOps ({max})")]
    PaymasterLimitReached { paymaster: Address, max: usize },

    /// Unstaked factory has too many UserOps
    #[error("Unstaked factory {factory} has reached maximum UserOps ({max})")]
    FactoryLimitReached { factory: Address, max: usize },

    /// Replacement gas price too low
    #[error("Replacement gas price {new_price} too low, need at least {required_price} (current: {current_price})")]
    ReplacementGasPriceTooLow {
        current_price: U256,
        new_price: U256,
        required_price: U256,
    },

    /// Entity is banned
    #[error("Entity {entity} is banned due to poor reputation")]
    EntityBanned { entity: Address },

    /// Entity is throttled (and random check failed)
    #[error("Entity {entity} is throttled, try again later")]
    EntityThrottled { entity: Address },

    /// Mempool rule violation
    #[error("Mempool rule {rule} violated: {description}")]
    MempoolRuleViolation {
        rule: Erc7562Rule,
        description: String,
        conflicting_hash: Option<B256>,
    },

    /// UserOp has expired
    #[error("UserOp expired: validUntil {valid_until} is in the past")]
    Expired { valid_until: u64 },

    /// Mempool is full and UserOp has lower priority than all existing
    #[error("Mempool full and UserOp gas price {gas_price} is below minimum {min_gas_price}")]
    MempoolFull { gas_price: U256, min_gas_price: U256 },

    /// Unsupported entrypoint
    #[error("Unsupported entrypoint: {0}")]
    UnsupportedEntrypoint(Address),

    /// UserOp not found
    #[error("UserOp {0} not found in mempool")]
    NotFound(B256),

    /// Validation error
    #[error("Validation error: {0}")]
    ValidationError(String),

    /// Internal error
    #[error("Internal mempool error: {0}")]
    Internal(String),
}

impl MempoolError {
    /// Create a sender limit error
    pub fn sender_limit(sender: Address, max: usize) -> Self {
        Self::SenderLimitReached { sender, max }
    }

    /// Create a paymaster limit error
    pub fn paymaster_limit(paymaster: Address, max: usize) -> Self {
        Self::PaymasterLimitReached { paymaster, max }
    }

    /// Create a factory limit error
    pub fn factory_limit(factory: Address, max: usize) -> Self {
        Self::FactoryLimitReached { factory, max }
    }

    /// Create a replacement gas price error
    pub fn replacement_gas_too_low(current: U256, new: U256, required: U256) -> Self {
        Self::ReplacementGasPriceTooLow {
            current_price: current,
            new_price: new,
            required_price: required,
        }
    }

    /// Create a mempool rule violation error
    pub fn rule_violation(
        rule: Erc7562Rule,
        description: impl Into<String>,
        conflicting_hash: Option<B256>,
    ) -> Self {
        Self::MempoolRuleViolation {
            rule,
            description: description.into(),
            conflicting_hash,
        }
    }

    /// Check if this error is due to a limit being reached (retriable)
    pub fn is_limit_error(&self) -> bool {
        matches!(
            self,
            Self::SenderLimitReached { .. }
                | Self::PaymasterLimitReached { .. }
                | Self::FactoryLimitReached { .. }
                | Self::MempoolFull { .. }
        )
    }

    /// Check if this error is due to reputation (potentially temporary)
    pub fn is_reputation_error(&self) -> bool {
        matches!(self, Self::EntityBanned { .. } | Self::EntityThrottled { .. })
    }
}
