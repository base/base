//! Error type shared by the L2 block and L1 head sources.

/// Errors produced by the L2 block and L1 head sources.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// Provider or RPC error.
    #[error("provider error: {0}")]
    Provider(String),
    /// A requested block has not become available from the provider yet.
    #[error("block {0} is not available yet")]
    BlockUnavailable(u64),
    /// The source will deliver nothing more: its channel or stream closed.
    #[error("source closed")]
    Closed,
}
