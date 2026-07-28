//! Error type for the [`EngineActor`].
//!
//! [`EngineActor`]: super::EngineActor

use base_consensus_engine::{EngineResetError, EngineTaskErrors};

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
    /// Engine task error.
    #[error(transparent)]
    EngineTask(#[from] EngineTaskErrors),
    /// A critical engine task error was already forwarded to the request caller.
    #[error("critical engine task error: {0}")]
    CriticalEngineTask(String),
    /// An uncoordinated processor reset orphaned an in-flight private shadow branch.
    #[error(
        "engine performed an internal reset while shadow sequencing; terminating to avoid a stale reconciliation gate"
    )]
    ShadowInternalReset,
    /// Shadow reconciliation failed after it may have mutated authoritative engine state.
    #[error("shadow reconciliation failed; terminating engine request handling: {0}")]
    ShadowReconciliationFailed(String),
}
