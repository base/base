//! Contains the error types used for finding the starting forkchoice state.

use alloy_eips::BlockId;
use alloy_primitives::B256;
use alloy_transport::{RpcError, TransportErrorKind};
use base_protocol::FromBlockError;
use thiserror::Error;

use super::{ForkchoiceCheckpointLabel, checkpoint::ForkchoiceCheckpointError};

/// An error that can occur during the sync start process.
#[derive(Error, Debug)]
pub enum SyncStartError {
    /// An rpc error occurred
    #[error("An RPC error occurred: {0}")]
    RpcError(#[from] RpcError<TransportErrorKind>),
    /// An error occurred while converting a block to [`L2BlockInfo`].
    ///
    /// [`L2BlockInfo`]: base_protocol::L2BlockInfo
    #[error(transparent)]
    FromBlock(#[from] FromBlockError),
    /// A block could not be found.
    #[error("Block not found: {0}")]
    BlockNotFound(BlockId),
    /// An error occurred while reading a forkchoice checkpoint.
    #[error(transparent)]
    ForkchoiceCheckpoint(#[from] ForkchoiceCheckpointError),
    /// Invalid L1 genesis hash.
    #[error("Invalid L1 genesis hash. Expected {0}, Got {1}")]
    InvalidL1GenesisHash(B256, B256),
    /// Invalid L2 genesis hash.
    #[error("Invalid L2 genesis hash. Expected {0}, Got {1}")]
    InvalidL2GenesisHash(B256, B256),
    /// Finalized block mismatch
    #[error("Finalized block mismatch. Expected {0}, Got {1}")]
    MismatchedFinalizedBlock(B256, B256),
    /// L1 origin mismatch.
    #[error("L1 origin mismatch")]
    L1OriginMismatch,
    /// Non-zero sequence number.
    #[error("Non-zero sequence number for block with different L1 origin")]
    NonZeroSequenceNumber,
    /// Inconsistent sequence number.
    #[error("Inconsistent sequence number; Must monotonically increase.")]
    InconsistentSequenceNumber,
    /// The on-disk forkchoice checkpoint did not match the reth-labeled head block.
    ///
    /// Surfaced instead of [`SyncStartError::FromBlock`] when the underlying
    /// [`FromBlockError::MissingL1InfoDeposit`] was caused by a stale or otherwise
    /// inconsistent checkpoint, so operators see "checkpoint mismatch" in logs rather
    /// than the misleading "missing L1 info deposit".
    #[error(
        "forkchoice checkpoint mismatch for {label}: reth labeled block {reth_number} ({reth_hash}), checkpoint {checkpoint_number} ({checkpoint_hash})"
    )]
    CheckpointMismatch {
        /// Which labeled head (safe / finalized) the mismatch was observed on.
        label: ForkchoiceCheckpointLabel,
        /// Block number reth returned for the label.
        reth_number: u64,
        /// Block hash reth returned for the label.
        reth_hash: B256,
        /// Block number recorded in the on-disk checkpoint.
        checkpoint_number: u64,
        /// Block hash recorded in the on-disk checkpoint.
        checkpoint_hash: B256,
    },
}
