//! Error type for the [`EngineActor`].
//!
//! [`EngineActor`]: super::EngineActor

use base_consensus_engine::{
    BuildTaskError, ConsolidateTaskError, DelegatedForkchoiceTaskError, EngineResetError,
    EngineTaskError, EngineTaskErrorSeverity, FinalizeTaskError, InsertTaskError, SealTaskError,
};

/// An error from a direct engine operation.
#[derive(thiserror::Error, Debug)]
pub enum EngineOperationError {
    /// An error that occurred while inserting a block into the engine.
    #[error(transparent)]
    Insert(#[from] InsertTaskError),
    /// An error that occurred while building a block.
    #[error(transparent)]
    Build(#[from] BuildTaskError),
    /// An error that occurred while sealing a block.
    #[error(transparent)]
    Seal(#[from] SealTaskError),
    /// An error that occurred while consolidating the engine state.
    #[error(transparent)]
    Consolidate(#[from] ConsolidateTaskError),
    /// An error that occurred while applying delegated follow-node forkchoice labels.
    #[error(transparent)]
    DelegatedForkchoice(#[from] DelegatedForkchoiceTaskError),
    /// An error that occurred while finalizing an L2 block.
    #[error(transparent)]
    Finalize(#[from] FinalizeTaskError),
}

impl EngineTaskError for EngineOperationError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::Insert(inner) => inner.severity(),
            Self::Build(inner) => inner.severity(),
            Self::Seal(inner) => inner.severity(),
            Self::Consolidate(inner) => inner.severity(),
            Self::DelegatedForkchoice(inner) => inner.severity(),
            Self::Finalize(inner) => inner.severity(),
        }
    }
}

/// An error from the [`EngineActor`].
///
/// [`EngineActor`]: super::EngineActor
#[derive(thiserror::Error, Debug)]
pub enum EngineError {
    /// Closed channel error.
    #[error("a channel has been closed unexpectedly")]
    ChannelClosed,
    /// Engine reset error.
    #[error(transparent)]
    EngineReset(#[from] EngineResetError),
    /// Engine operation error.
    #[error(transparent)]
    EngineOperation(#[from] EngineOperationError),
    /// A critical engine operation error was already forwarded to the request caller.
    #[error("critical engine operation error: {0}")]
    CriticalEngineOperation(String),
}
