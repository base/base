//! Error type for the [`EngineActor`].
//!
//! [`EngineActor`]: super::EngineActor

use base_consensus_engine::{EngineResetError, EngineTaskErrors};

/// An error from the [`EngineActor`].
///
/// [`EngineActor`]: super::EngineActor
#[derive(thiserror::Error, Debug)]
pub enum EngineError {
    /// Historical startup failed without releasing its activation gate.
    #[error(transparent)]
    HistoricalStartup(#[from] HistoricalStartupFailure),
    /// Closed channel error.
    #[error("a channel has been closed unexpectedly")]
    ChannelClosed,
    /// The execution head could not be loaded during engine bootstrap.
    #[error("failed to query execution head during bootstrap: {0}")]
    BootstrapHeadQuery(String),
    /// Engine reset error.
    #[error(transparent)]
    EngineReset(#[from] EngineResetError),
    /// Engine task error.
    #[error(transparent)]
    EngineTask(#[from] EngineTaskErrors),
    /// A critical engine task error was already forwarded to the request caller.
    #[error("critical engine task error: {0}")]
    CriticalEngineTask(String),
    /// An automatic processor reset invalidated shadow reconciliation state.
    #[error(
        "engine performed an internal reset while shadow sequencing; terminating to avoid a stale reconciliation gate"
    )]
    ShadowInternalReset,
    /// Shadow reconciliation failed after it may have mutated authoritative engine state.
    #[error("shadow reconciliation failed; terminating engine request handling: {0}")]
    ShadowReconciliationFailed(String),
}

/// A classified historical-startup failure with its original error source preserved.
#[derive(thiserror::Error, Debug)]
pub enum HistoricalStartupFailure {
    /// No historical target was selected before probing.
    #[error("historical startup was not selected")]
    NotSelected,
    /// The caller supplied no remaining probe budget.
    #[error("historical startup probe budget exhausted")]
    BudgetExhausted,
    /// The single overall startup deadline elapsed.
    #[error("historical startup deadline exceeded")]
    DeadlineExceeded,
    /// Strict non-leader admission refused the configured leader.
    #[error("historical startup requires a non-leader")]
    Leader,
    /// The required leadership query exceeded its bounded wait.
    #[error("historical startup leadership query timed out")]
    LeadershipTimeout,
    /// The required leadership query failed; do not assume follower status.
    #[error("historical startup leadership query failed: {0}")]
    Leadership(#[source] Box<crate::ConductorError>),
    /// A target confirmation failed with its typed request or status error.
    #[error("historical startup target confirmation failed: {0}")]
    Probe(#[source] Box<base_consensus_engine::StartupSyncError>),
    /// The initial local-head query exceeded its bounded admission wait.
    #[error("historical startup head query timed out")]
    HeadQueryTimeout,
    /// Execution-head observation failed before admission or handoff.
    #[error("historical startup head query failed: {0}")]
    HeadQuery(#[source] Box<base_consensus_engine::EngineClientError>),
    /// An absent execution head cannot establish safe cold-start admission.
    #[error("historical startup requires an available local execution head")]
    MissingHead,
    /// Repeated configuration must not send a stale target to the engine.
    #[error("historical startup target {target} is not ahead of local head {local}; remove the single-use historical EL sync target option before restarting")]
    ReusedTarget {
        /// Configured one-shot target height.
        target: u64,
        /// Local execution height observed before any startup FCU.
        local: u64,
    },
    /// The canonical head no longer matches the execution-confirmed target.
    #[error("historical startup target is no longer canonical")]
    TargetChanged,
    /// Sync-start selection changed the confirmed unsafe head.
    #[error("historical startup reset changed the target")]
    ResetChangedTarget,
    /// The label request failed; preserve its transport or JSON-RPC error.
    #[error("historical startup label forkchoice request failed: {0}")]
    LabelRequest(#[source] Box<alloy_transport::TransportError>),
    /// A label response other than VALID or SYNCING is terminal.
    #[error("unexpected historical startup label forkchoice status: {0}")]
    LabelStatus(alloy_rpc_types_engine::PayloadStatusEnum),
    /// Handoff did not complete within the original phase deadline.
    #[error("historical startup handoff timed out")]
    HandoffTimeout,
}
