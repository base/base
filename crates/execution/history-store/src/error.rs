//! Error type shared by every history store component.

use alloy_primitives::B256;
use reth_provider::ProviderError;

use crate::VerifyError;

/// Errors returned by the history store writer, pruner, garbage collector and reader.
#[derive(Debug, thiserror::Error)]
pub enum HistoryStoreError {
    /// A wire row could not be decoded into reth's types.
    #[error("failed to decode {what} for block {block}: {reason}")]
    Decode {
        /// What was being decoded.
        what: &'static str,
        /// Block the row belongs to.
        block: u64,
        /// Decoder message.
        reason: String,
    },
    /// A static file could not be written or read.
    #[error("static file error: {0}")]
    StaticFile(#[from] ProviderError),
    /// Local filesystem I/O failed (temporary build directory or cache).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A segment was given the wrong set of blocks.
    #[error("segment {segment} expects blocks {first}..={last}, got {reason}")]
    SegmentShape {
        /// Segment number.
        segment: u64,
        /// First block expected.
        first: u64,
        /// Last block expected.
        last: u64,
        /// What was wrong.
        reason: String,
    },
    /// A downloaded segment file is corrupt or inconsistent.
    #[error("corrupt segment {segment} file {file}: {reason}")]
    CorruptSegment {
        /// Segment number.
        segment: u64,
        /// File name.
        file: String,
        /// What was wrong.
        reason: String,
    },
    /// The hash of a block did not match what the caller expected.
    #[error("block {block} hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        /// Block number.
        block: u64,
        /// Expected hash.
        expected: B256,
        /// Actual hash.
        actual: B256,
    },
    /// The node payload failed verification against its headers.
    #[error("verification failed: {0}")]
    Verify(#[from] VerifyError),
    /// An object store call failed.
    #[error("object store error: {0}")]
    ObjectStore(#[from] object_store::Error),
    /// A `SlateDB` call failed.
    #[error("index error: {0}")]
    Slate(#[from] slatedb::Error),
    /// An index entry is corrupt.
    #[error("index error: {reason}")]
    Index {
        /// What was wrong.
        reason: String,
    },
    /// The catalog is malformed or violates an invariant.
    #[error("catalog error: {reason}")]
    Catalog {
        /// What was wrong.
        reason: String,
    },
    /// The catalog belongs to another chain or format.
    #[error("catalog {field} mismatch: expected {expected}, got {actual}")]
    CatalogMismatch {
        /// Field that differs.
        field: &'static str,
        /// Value the caller expected.
        expected: String,
        /// Value in the catalog.
        actual: String,
    },
    /// A newer writer, pruner or GC claimed the archive; this instance must stop (TDD §7 single
    /// writer). `owned` is the epoch this instance claimed, `current` the catalog's.
    #[error("fenced: archive owned by writer epoch {current}, this instance holds {owned}")]
    Fenced {
        /// Epoch this instance claimed.
        owned: u64,
        /// Epoch recorded in the catalog.
        current: u64,
    },
    /// Even a one-block `archive_getBlockRange` exceeds a source or transport limit.
    #[error("block {block} exceeds every source's range limit")]
    RangeTooLarge {
        /// Block that cannot be fetched alone.
        block: u64,
    },
    /// A source rejected the range as over its block or byte cap (`4003`, `4004`, or an
    /// oversized response); the caller should retry with a smaller chunk.
    #[error("source {source_name} rejected {from}..={to} as too large: {reason}")]
    SourceRangeTooLarge {
        /// Source name.
        source_name: String,
        /// First block requested.
        from: u64,
        /// Last block requested.
        to: u64,
        /// Source message.
        reason: String,
    },
    /// Another writer changed the catalog first (HTTP 412).
    #[error("catalog compare-and-swap lost the race")]
    CasConflict,
    /// The object store backend cannot do conditional updates (for example `LocalFileSystem`).
    #[error("object store backend does not support conditional catalog updates")]
    BackendUnsupported,
    /// The archive configuration (URL, endpoint, sizes) is invalid.
    #[error("invalid archive configuration: {reason}")]
    InvalidConfig {
        /// What is wrong.
        reason: String,
    },
    /// A catalog update would move `H` or `E` backwards or skip a generation.
    #[error("catalog regression: {reason}")]
    Regression {
        /// What would regress.
        reason: String,
    },
    /// The writer stopped publication because a finalized hash changed under the archive.
    #[error("halted: block {block} finalized hash diverged (archive {archive}, node {node})")]
    Halt {
        /// Block whose hash differs.
        block: u64,
        /// Hash the archive or first fetch holds.
        archive: B256,
        /// Hash the confirming node returned.
        node: B256,
    },
    /// A node source failed or refused the request.
    #[error("source error: {0}")]
    Source(String),
    /// Every configured node failed for this range.
    #[error("no node could serve blocks {from}..={to}")]
    NoSource {
        /// First block requested.
        from: u64,
        /// Last block requested.
        to: u64,
    },
    /// The requested history is older than the archive's earliest retained block.
    #[error("history pruned: block {block} is below archive earliest block {earliest}")]
    HistoryPruned {
        /// Requested block.
        block: u64,
        /// Earliest retained block `E`.
        earliest: u64,
    },
    /// The archive cannot serve this request (catalog unreachable or block above `H`).
    #[error("archive unavailable: {reason}")]
    Unavailable {
        /// Why.
        reason: String,
    },
}
