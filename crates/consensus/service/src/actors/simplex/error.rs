//! Simplex consensus actor error types.

/// Error returned by simplex consensus actor operations.
#[derive(Debug, thiserror::Error)]
pub enum SimplexError {
    /// The simplex actor request channel was closed.
    #[error("simplex actor channel closed")]
    ChannelClosed,
    /// The simplex actor response was dropped before a reply was sent.
    #[error("simplex actor response dropped")]
    ResponseDropped,
    /// The requested operation is not yet implemented.
    ///
    /// Returned by the Phase 1 skeleton actor, which wires the request/response
    /// plumbing but carries no consensus logic yet. Replaced as the commonware
    /// simplex engine is integrated in Phase 2.
    #[error("simplex consensus not yet implemented (Phase 1 skeleton)")]
    NotImplemented,
    /// A consensus-layer error surfaced from the underlying engine.
    #[error("simplex consensus error: {0}")]
    Consensus(String),
}
