//! Handle to a running unified rollup node.

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::UnifiedRollupNodeError;

/// Handle to a running unified rollup node.
///
/// Use [`NodeHandle::wait`] to wait for the node to complete, or
/// [`NodeHandle::cancel`] to initiate shutdown.
#[derive(Debug)]
pub struct NodeHandle {
    engine_handle: tokio::task::JoinHandle<Result<(), UnifiedRollupNodeError>>,
    shutdown_handle: tokio::task::JoinHandle<()>,
    cancellation_token: CancellationToken,
}

impl NodeHandle {
    /// Creates a new `NodeHandle`.
    pub const fn new(
        engine_handle: tokio::task::JoinHandle<Result<(), UnifiedRollupNodeError>>,
        shutdown_handle: tokio::task::JoinHandle<()>,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self { engine_handle, shutdown_handle, cancellation_token }
    }

    /// Returns a reference to the cancellation token.
    ///
    /// This can be used to cancel the node from another task.
    pub const fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }

    /// Cancels the node, initiating shutdown.
    pub fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    /// Waits for the node to complete.
    pub async fn wait(self) -> Result<(), UnifiedRollupNodeError> {
        tokio::select! {
            result = self.engine_handle => {
                match result {
                    Ok(Ok(())) => {
                        info!("Engine actor completed successfully");
                    }
                    Ok(Err(e)) => {
                        warn!(%e, "Engine actor failed");
                        self.cancellation_token.cancel();
                        return Err(e);
                    }
                    Err(join_error) => {
                        warn!(%join_error, "Engine actor task panicked");
                        self.cancellation_token.cancel();
                        return Err(UnifiedRollupNodeError::EngineActorFailed(
                            join_error.to_string(),
                        ));
                    }
                }
            }
            _ = self.cancellation_token.cancelled() => {
                info!("Unified rollup node cancelled");
            }
        }

        // Cancel the shutdown handler if it's still running
        self.shutdown_handle.abort();

        info!("Unified rollup node stopped");
        Ok(())
    }
}
