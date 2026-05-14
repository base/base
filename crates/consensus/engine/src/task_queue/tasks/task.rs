//! Tasks sent to the [`Engine`] for execution.
//!
//! [`Engine`]: crate::Engine

use std::cmp::Ordering;

use async_trait::async_trait;
use derive_more::Display;
use thiserror::Error;
use tokio::task::yield_now;

use super::{DelegatedForkchoiceTask, FinalizeTask, SealTaskError};
use crate::{
    BuildTaskError, ConsolidateTaskError, DelegatedForkchoiceTaskError, EngineClient, EngineState,
    FinalizeTaskError, InsertTaskError, Metrics,
};

/// The severity of an engine task error.
///
/// This is used to determine how to handle the error when draining the engine task queue.
#[derive(Debug, PartialEq, Eq, Display, Clone, Copy)]
pub enum EngineTaskErrorSeverity {
    /// The error is temporary and the task is retried.
    #[display("temporary")]
    Temporary,
    /// The error is critical and is propagated to the engine actor.
    #[display("critical")]
    Critical,
    /// The error indicates that the engine should be reset.
    #[display("reset")]
    Reset,
    /// The error indicates that the engine should be flushed.
    #[display("flush")]
    Flush,
}

/// The interface for an engine task error.
///
/// An engine task error should have an associated severity level to specify how to handle the error
/// when draining the engine task queue.
pub trait EngineTaskError {
    /// The severity of the error.
    fn severity(&self) -> EngineTaskErrorSeverity;
}

/// The interface for an engine task.
#[async_trait]
pub trait EngineTaskExt {
    /// The output type of the task.
    type Output;

    /// The error type of the task.
    type Error: EngineTaskError;

    /// Executes the task, taking a shared lock on the engine state and `self`.
    async fn execute(&self, state: &mut EngineState) -> Result<Self::Output, Self::Error>;
}

/// An error that may occur during an [`EngineTask`]'s execution.
#[derive(Error, Debug)]
pub enum EngineTaskErrors {
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

impl EngineTaskErrorSeverity {
    /// Returns a static string label for use in metrics.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Temporary => "temporary",
            Self::Critical => "critical",
            Self::Reset => "reset",
            Self::Flush => "flush",
        }
    }
}

impl EngineTaskError for EngineTaskErrors {
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

/// Tasks that may be inserted into and executed by the [`Engine`].
///
/// [`Engine`]: crate::Engine
#[derive(Debug, Clone)]
pub enum EngineTask<EngineClient_: EngineClient> {
    /// Applies delegated safe and finalized labels for follow mode.
    DelegatedForkchoice(Box<DelegatedForkchoiceTask<EngineClient_>>),
    /// Finalizes an L2 block
    Finalize(Box<FinalizeTask<EngineClient_>>),
}

impl<EngineClient_: EngineClient> EngineTask<EngineClient_> {
    /// Executes the task without consuming it.
    async fn execute_inner(&self, state: &mut EngineState) -> Result<(), EngineTaskErrors> {
        match self {
            Self::DelegatedForkchoice(task) => task.execute(state).await?,
            Self::Finalize(task) => task.execute(state).await?,
        };

        Ok(())
    }

    const fn task_metrics_label(&self) -> &'static str {
        match self {
            Self::DelegatedForkchoice(_) => Metrics::DELEGATED_FORKCHOICE_TASK_LABEL,
            Self::Finalize(_) => Metrics::FINALIZE_TASK_LABEL,
        }
    }
}

impl<EngineClient_: EngineClient> PartialEq for EngineTask<EngineClient_> {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::DelegatedForkchoice(_), Self::DelegatedForkchoice(_))
                | (Self::Finalize(_), Self::Finalize(_))
        )
    }
}

impl<EngineClient_: EngineClient> Eq for EngineTask<EngineClient_> {}

impl<EngineClient_: EngineClient> PartialOrd for EngineTask<EngineClient_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<EngineClient_: EngineClient> Ord for EngineTask<EngineClient_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Order (descending): Delegated forkchoice -> Finalize
        //
        // https://specs.base.org/protocol/consensus/derivation#forkchoice-synchronization
        //
        // - Delegated forkchoice tasks are prioritized over Finalize tasks, as they advance the
        //   safe chain via derivation.
        // - Finalize tasks have the lowest priority, as they only update finalized status.
        match (self, other) {
            // Same variant cases
            (Self::DelegatedForkchoice(_), Self::DelegatedForkchoice(_))
            | (Self::Finalize(_), Self::Finalize(_)) => Ordering::Equal,
            (Self::DelegatedForkchoice(_), Self::Finalize(_)) => Ordering::Greater,
            (Self::Finalize(_), Self::DelegatedForkchoice(_)) => Ordering::Less,
        }
    }
}

#[async_trait]
impl<EngineClient_: EngineClient> EngineTaskExt for EngineTask<EngineClient_> {
    type Output = ();

    type Error = EngineTaskErrors;

    async fn execute(&self, state: &mut EngineState) -> Result<(), Self::Error> {
        // Wall-clock duration of the entire retry loop (not per attempt), so the
        // difference against `engine_method_request_duration{method}` isolates pure
        // CL retry/yield overhead. Records on drop regardless of outcome
        // (success / critical / reset / flush).
        let label = self.task_metrics_label();
        let _task_timer = base_metrics::timed!(Metrics::engine_task_duration(label));

        // Retry the task until it succeeds or a critical error occurs.
        while let Err(e) = self.execute_inner(state).await {
            let severity = e.severity();

            Metrics::engine_task_failure(self.task_metrics_label(), severity.as_label())
                .increment(1);

            match severity {
                EngineTaskErrorSeverity::Temporary => {
                    trace!(target: "engine", error = %e, "Temporary engine error");

                    // Yield the task to allow other tasks to execute to avoid starvation.
                    yield_now().await;

                    continue;
                }
                EngineTaskErrorSeverity::Critical => {
                    error!(target: "engine", error = %e, "Critical engine error");
                    return Err(e);
                }
                EngineTaskErrorSeverity::Reset => {
                    warn!(target: "engine", "Engine requested derivation reset");
                    return Err(e);
                }
                EngineTaskErrorSeverity::Flush => {
                    warn!(target: "engine", "Engine requested derivation flush");
                    return Err(e);
                }
            }
        }

        Metrics::engine_task_count(self.task_metrics_label()).increment(1);

        Ok(())
    }
}
