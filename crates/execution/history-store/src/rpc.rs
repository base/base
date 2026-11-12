//! `archive_` JSON-RPC namespace served by every node (TDD §3).
//!
//! The server side is implemented by `base-history-rpc`; the writer uses the generated client.

use jsonrpsee::{
    core::RpcResult,
    proc_macros::rpc,
    types::{ErrorObject, ErrorObjectOwned},
};
use serde::{Deserialize, Serialize};

use crate::ArchiveBlock;

/// Default cap on blocks per `archive_getBlockRange` call.
pub const DEFAULT_MAX_RANGE_BLOCKS: u64 = 8192;
/// Default cap on payload bytes per `archive_getBlockRange` call (64 MiB).
///
/// Deviation from the TDD's 256 MB: the cap counts raw payload bytes, but the response is hex
/// JSON (about 2x), and reth's default RPC response limit is 160 MiB. 64 MiB keeps a full range
/// under that limit.
pub const DEFAULT_MAX_RANGE_BYTES: u64 = 64 * 1024 * 1024;

/// Response of `archive_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveStatus {
    /// Canonical head block.
    pub head: u64,
    /// Safe block.
    pub safe: u64,
    /// Finalized block.
    pub finalized: u64,
    /// Earliest block whose full export data the node still holds.
    pub earliest_local: u64,
    /// Cap on blocks per range call.
    pub max_range_blocks: u64,
    /// Cap on payload bytes per range call.
    pub max_range_bytes: u64,
}

/// Bounds echoed back with range errors so the writer can back off or pick another node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeBounds {
    /// Node's finalized block.
    pub finalized: u64,
    /// Node's earliest local block.
    pub earliest_local: u64,
}

/// Typed `archive_getBlockRange` errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArchiveRpcError {
    /// `to` is above the node's finalized block.
    #[error("range end {to} is not finalized")]
    NotFinalized {
        /// Requested end.
        to: u64,
        /// Node bounds.
        bounds: RangeBounds,
    },
    /// `from` is below the node's earliest local block.
    #[error("range start {from} is pruned")]
    Pruned {
        /// Requested start.
        from: u64,
        /// Node bounds.
        bounds: RangeBounds,
    },
    /// Too many blocks requested.
    #[error("range of {blocks} blocks exceeds maxRangeBlocks {max}")]
    TooManyBlocks {
        /// Blocks requested.
        blocks: u64,
        /// Cap.
        max: u64,
    },
    /// The response would exceed the byte cap.
    #[error("range exceeds maxRangeBytes {max} at block {block}")]
    TooManyBytes {
        /// Block at which the cap was crossed.
        block: u64,
        /// Cap.
        max: u64,
    },
    /// `from > to`.
    #[error("invalid range {from}..={to}")]
    InvalidRange {
        /// Requested start.
        from: u64,
        /// Requested end.
        to: u64,
    },
    /// A height in range is missing or not canonical on this node.
    #[error("block {block} not found")]
    NotFound {
        /// Missing block.
        block: u64,
    },
    /// The node pruned the changesets for a block in range.
    #[error("changesets for block {block} are unavailable")]
    ChangesetsUnavailable {
        /// Block whose changesets are missing.
        block: u64,
    },
    /// Provider failure.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ArchiveRpcError {
    /// JSON-RPC error code of `NotFinalized`.
    pub const NOT_FINALIZED: i32 = 4001;
    /// JSON-RPC error code of `Pruned`.
    pub const PRUNED: i32 = 4002;
    /// JSON-RPC error code of `TooManyBlocks`.
    pub const TOO_MANY_BLOCKS: i32 = 4003;
    /// JSON-RPC error code of `TooManyBytes`.
    pub const TOO_MANY_BYTES: i32 = 4004;
    /// JSON-RPC error code of `InvalidRange`.
    pub const INVALID_RANGE: i32 = 4005;
    /// JSON-RPC error code of `NotFound`.
    pub const NOT_FOUND: i32 = 4006;
    /// JSON-RPC error code of `ChangesetsUnavailable`.
    pub const CHANGESETS_UNAVAILABLE: i32 = 4007;
    /// JSON-RPC error code of `Internal`.
    pub const INTERNAL: i32 = -32603;

    /// Returns the JSON-RPC error code.
    pub const fn code(&self) -> i32 {
        match self {
            Self::NotFinalized { .. } => Self::NOT_FINALIZED,
            Self::Pruned { .. } => Self::PRUNED,
            Self::TooManyBlocks { .. } => Self::TOO_MANY_BLOCKS,
            Self::TooManyBytes { .. } => Self::TOO_MANY_BYTES,
            Self::InvalidRange { .. } => Self::INVALID_RANGE,
            Self::NotFound { .. } => Self::NOT_FOUND,
            Self::ChangesetsUnavailable { .. } => Self::CHANGESETS_UNAVAILABLE,
            Self::Internal(_) => Self::INTERNAL,
        }
    }
}

impl From<ArchiveRpcError> for ErrorObjectOwned {
    fn from(err: ArchiveRpcError) -> Self {
        let message = err.to_string();
        match &err {
            ArchiveRpcError::NotFinalized { bounds, .. } | ArchiveRpcError::Pruned { bounds, .. } => {
                ErrorObject::owned(err.code(), message, Some(*bounds))
            }
            _ => ErrorObject::owned::<()>(err.code(), message, None),
        }
    }
}

/// The `archive_` namespace.
#[rpc(server, client, namespace = "archive")]
pub trait ArchiveApi {
    /// Returns the node's head, safe and finalized blocks, earliest local block and range caps.
    #[method(name = "status")]
    async fn status(&self) -> RpcResult<ArchiveStatus>;

    /// Returns blocks `from..=to` with their export rows.
    #[method(name = "getBlockRange")]
    async fn get_block_range(&self, from: u64, to: u64) -> RpcResult<Vec<ArchiveBlock>>;
}
