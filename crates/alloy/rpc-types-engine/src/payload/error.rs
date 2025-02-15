//! Optimism payload errors.

use alloy_rpc_types_engine::PayloadError;

/// Extends [`PayloadError`] for Optimism.
#[derive(Debug, thiserror::Error)]
pub enum OpPayloadError {
    /// Non-empty list of execution layer requests.
    #[error("non-empty EL requests")]
    NonEmptyELRequests,
    /// Non-empty list of blob versioned hashes.
    #[error("non-empty blob versioned hashes")]
    NonEmptyBlobVersionedHashes,
    /// L1 [`PayloadError`] that can also occur on L2.
    #[error(transparent)]
    Eth(#[from] PayloadError),
}
