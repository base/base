//! Payload errors.

use alloy_primitives::B256;
use alloy_rpc_types_engine::PayloadError;

/// Extends [`PayloadError`].
#[derive(Debug, thiserror::Error)]
pub enum BasePayloadError {
    /// Non-empty list of L1 withdrawals (Shanghai).
    #[error("non-empty L1 withdrawals")]
    NonEmptyL1Withdrawals,
    /// Contains unsupported blob transaction type EIP-4844.
    #[error("contains blob transaction")]
    BlobTransaction,
    /// Non-empty list of execution layer requests.
    #[error("non-empty EL requests")]
    NonEmptyELRequests,
    /// Non-empty list of blob versioned hashes.
    #[error("non-empty blob versioned hashes")]
    NonEmptyBlobVersionedHashes,
    /// The block access list carried by the payload does not hash to the commitment in the
    /// block header (EIP-7928).
    #[error("block access list hash mismatch: header {header}, payload {payload}")]
    BlockAccessListHashMismatch {
        /// Hash committed by the block header.
        header: B256,
        /// Hash of the block access list carried by the payload.
        payload: B256,
    },
    /// The block access list carried by the payload is not valid RLP.
    #[error("invalid block access list: {0}")]
    InvalidBlockAccessList(alloy_rlp::Error),
    /// L1 [`PayloadError`] that can also occur on L2.
    #[error(transparent)]
    Eth(#[from] PayloadError),
}
