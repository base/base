use base_proof_preimage::PreimageKey;
use thiserror::Error;

/// Errors returned while constructing a proof witness.
#[derive(Debug, Error)]
pub enum WitnessError {
    /// A content-addressed value did not match its key.
    #[error("preimage does not match key {0}")]
    InvalidPreimage(PreimageKey),
    /// Two sources returned different values for one key.
    #[error("conflicting values for preimage key {0}")]
    ConflictingPreimage(PreimageKey),
    /// An upstream RPC request failed.
    #[error("{operation} failed: {error}")]
    Rpc {
        /// Operation that failed.
        operation: &'static str,
        /// Upstream error.
        error: String,
    },
    /// A requested block was unavailable.
    #[error("{layer} block {number} was not found")]
    BlockNotFound {
        /// Chain layer.
        layer: &'static str,
        /// Block number.
        number: u64,
    },
    /// A hash-pinned block was unavailable.
    #[error("{layer} block {hash} was not found")]
    BlockHashNotFound {
        /// Chain layer.
        layer: &'static str,
        /// Requested block hash.
        hash: alloy_primitives::B256,
    },
    /// A fetched block did not match its requested number or hash.
    #[error("invalid {layer} block {number}: {reason}")]
    InvalidBlock {
        /// Chain layer.
        layer: &'static str,
        /// Block number.
        number: u64,
        /// Validation failure.
        reason: String,
    },
    /// The proof request describes an invalid block range.
    #[error("invalid proof range: agreed L2 block {agreed} is after claimed block {claimed}")]
    InvalidRange {
        /// Agreed L2 block number.
        agreed: u64,
        /// Claimed L2 block number.
        claimed: u64,
    },
    /// The agreed L2 output root did not match the reconstructed value.
    #[error("agreed L2 output root mismatch")]
    OutputRootMismatch,
    /// L1 ancestry did not connect the derivation origin to the pinned head.
    #[error("invalid L1 ancestry: {0}")]
    InvalidL1Ancestry(String),
    /// A response could not be encoded or decoded.
    #[error("encoding failed: {0}")]
    Encoding(String),
    /// Blob sidecars did not match the requested hashes.
    #[error("expected {expected} blobs, received {actual}")]
    BlobCountMismatch {
        /// Expected sidecar count.
        expected: usize,
        /// Actual sidecar count.
        actual: usize,
    },
}

/// Result type for witness generation.
pub type Result<T> = core::result::Result<T, WitnessError>;
