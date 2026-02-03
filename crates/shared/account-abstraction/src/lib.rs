//! Account abstraction types and utilities for ERC-4337.
//!
//! This crate provides types and utilities for working with ERC-4337 account abstraction,
//! including user operations, mempools, and reputation services.

/// `EntryPoint` contract definitions and hash calculation for different ERC-4337 versions.
pub mod entrypoints;
/// Events emitted by the mempool.
pub mod events;
/// Mempool trait and configuration for user operations.
pub mod mempool;
/// Reputation tracking for entity behavior.
pub mod reputation;
/// Core types for user operations and validation.
pub mod types;

pub use events::MempoolEvent;
pub use mempool::{Mempool, PoolConfig};
pub use reputation::{ReputationService, ReputationStatus};
pub use types::{
    UserOpHash, UserOperationRequest, ValidationResult, VersionedUserOperation,
    WrappedUserOperation,
};
