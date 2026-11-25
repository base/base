//! Errors returned by the archive format, publisher and reader.

use alloy_primitives::B256;

use crate::Family;

/// Errors returned by the Exoware archive.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArchiveError {
    /// A store request failed or timed out. The outcome of a failed write is unknown: the
    /// store may have applied it.
    #[error("store {op} failed: {reason}")]
    Store {
        /// Store operation (`put`, `get`, `get_many`, `range`, `prune`).
        op: &'static str,
        /// Client error text.
        reason: String,
    },
    /// The config row is absent; an operator must provision it first.
    #[error("archive config row is missing; provision it first")]
    ConfigMissing,
    /// The stored config row differs from the expected one.
    #[error("archive config mismatch: stored {stored}, expected {expected}")]
    ConfigMismatch {
        /// Stored config, rendered.
        stored: String,
        /// Expected config, rendered.
        expected: String,
    },
    /// A config value is out of range.
    #[error("invalid archive config: {reason}")]
    InvalidConfig {
        /// Why the config is invalid.
        reason: String,
    },
    /// The archive belongs to another chain; the reader disables the archive link.
    #[error("archive link disabled: {reason}")]
    LinkDisabled {
        /// Which identity field differs.
        reason: String,
    },
    /// Segment `k` cannot be published because marker `k − 1` is absent.
    #[error("segment {segment} cannot be published before segment {missing}")]
    Contiguity {
        /// Segment being published.
        segment: u64,
        /// Missing predecessor.
        missing: u64,
    },
    /// Marker `k` exists with different contents; export halts and nothing is overwritten.
    #[error(
        "segment {segment} marker differs from the locally built one (stored digest {stored}, local {local}); export quarantined"
    )]
    Quarantined {
        /// Segment number.
        segment: u64,
        /// Digest in the stored marker.
        stored: B256,
        /// Digest of the locally built rows.
        local: B256,
    },
    /// Segment `k`'s first block does not extend marker `k − 1`: either the local chain or the
    /// stored predecessor marker is wrong. Export halts and nothing of segment `k` is written.
    #[error(
        "segment {segment} first block parent {parent_hash} does not match marker {} last hash {predecessor_hash}; export quarantined",
        segment.saturating_sub(1)
    )]
    LinkageQuarantined {
        /// Segment being published.
        segment: u64,
        /// `last_hash` of marker `segment − 1`.
        predecessor_hash: B256,
        /// Parent hash of the segment's first block.
        parent_hash: B256,
    },
    /// The segment ends above the node's finalized block.
    #[error("segment {segment} ends at block {last_block}, above finalized block {finalized:?}")]
    NotFinalized {
        /// Segment number.
        segment: u64,
        /// Last block of the segment.
        last_block: u64,
        /// Node's finalized block.
        finalized: Option<u64>,
    },
    /// A row value exceeds the store's 32 MiB value limit; the segment cannot be published.
    #[error("row {key} of segment {segment} has {len} bytes, above the {max}-byte value limit")]
    ValueTooLarge {
        /// Segment number.
        segment: u64,
        /// Hex-encoded logical key.
        key: String,
        /// Value length.
        len: usize,
        /// Store limit.
        max: usize,
    },
    /// The node no longer has the data needed to export a block.
    #[error("local {table} for block {block} is pruned (earliest local block {earliest})")]
    LocalDataPruned {
        /// Table name.
        table: &'static str,
        /// Block requested.
        block: u64,
        /// Earliest locally available block of that table.
        earliest: u64,
    },
    /// The node failed to read local data.
    #[error("local read failed: {reason}")]
    Local {
        /// Provider error text.
        reason: String,
    },
    /// Block data handed to the row builder is inconsistent.
    #[error("invalid block {block}: {reason}")]
    InvalidBlock {
        /// Block number.
        block: u64,
        /// What is wrong.
        reason: String,
    },
    /// A stored value cannot be decoded.
    #[error("cannot decode {family:?} value: {reason}")]
    Decode {
        /// Family of the row.
        family: Family,
        /// Decoder error text.
        reason: String,
    },
    /// The block is above the highest published segment; the caller serves it locally.
    #[error("block {block} is not archived (highest archived block {highest:?})")]
    NotArchived {
        /// Block requested.
        block: u64,
        /// Highest archived block, if any segment is published.
        highest: Option<u64>,
    },
    /// The block is below the archive's retention window, or a prune removed it during the
    /// request.
    #[error("block {block} is pruned from the archive (earliest archived block {earliest})")]
    Pruned {
        /// Block requested.
        block: u64,
        /// Earliest block the archive still serves.
        earliest: u64,
    },
    /// The archive returned missing or unverifiable data for a block it claims to hold.
    #[error("archive {family:?} unavailable: {reason}")]
    Unavailable {
        /// Family that failed.
        family: Family,
        /// What failed.
        reason: String,
    },
    /// A log query exceeds the archive's limits.
    #[error("log query exceeds archive limits: {reason}")]
    LimitExceeded {
        /// Which limit.
        reason: String,
    },
}

impl ArchiveError {
    /// Builds a [`ArchiveError::Store`] from a client error.
    pub fn store(op: &'static str, err: impl std::fmt::Display) -> Self {
        Self::Store { op, reason: err.to_string() }
    }

    /// Builds a [`ArchiveError::Unavailable`] for `family`.
    pub fn unavailable(family: Family, reason: impl Into<String>) -> Self {
        Self::Unavailable { family, reason: reason.into() }
    }

    /// Builds a [`ArchiveError::Decode`] for `family`.
    pub fn decode(family: Family, reason: impl std::fmt::Display) -> Self {
        Self::Decode { family, reason: reason.to_string() }
    }
}
