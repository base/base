//! Error types for the delegated forkchoice task.

use thiserror::Error;

use crate::{
    ConsolidateTaskError, EngineTaskError, FinalizeTaskError,
    task_queue::tasks::task::EngineTaskErrorSeverity,
};

/// An error returned by the delegated follow-node forkchoice task.
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
