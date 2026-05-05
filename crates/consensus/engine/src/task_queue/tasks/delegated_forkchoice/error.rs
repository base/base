//! Error types for delegated forkchoice updates.

use thiserror::Error;

use crate::{ConsolidateTaskError, EngineTaskError, EngineTaskErrorSeverity, FinalizeTaskError};

/// An error returned by a delegated follow-node forkchoice update.
#[derive(Debug, Error)]
pub enum DelegatedForkchoiceTaskError {
    /// Consolidation failed while applying the delegated safe head.
    #[error(transparent)]
    Consolidate(#[from] ConsolidateTaskError),
    /// Finalization failed while advancing the delegated finalized head.
    #[error(transparent)]
    Finalize(#[from] FinalizeTaskError),
}

impl EngineTaskError for DelegatedForkchoiceTaskError {
    fn severity(&self) -> EngineTaskErrorSeverity {
        match self {
            Self::Consolidate(inner) => inner.severity(),
            Self::Finalize(inner) => inner.severity(),
        }
    }
}
