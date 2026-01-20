//! Request processor for the engine actor.

use std::{fmt, sync::Arc};

use base_engine_ext::DirectEngineApi;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, trace, warn};

use crate::{
    EngineActorError, EngineActorRequest, EngineSyncState,
    handlers::{
        BuildHandler, ConsolidationHandler, FinalizationHandler, ResetHandler, SealHandler,
        UnsafeBlockHandler,
    },
};

/// Processes engine actor requests.
///
/// This struct runs the main request processing loop, dispatching
/// requests to the appropriate handlers.
///
/// # Type Parameters
///
/// * `E` - The engine API implementation, must implement [`DirectEngineApi`]
pub struct DirectEngineProcessor<E: DirectEngineApi> {
    /// The engine client implementing DirectEngineApi.
    client: Arc<E>,
    /// The request receiver.
    rx: mpsc::Receiver<EngineActorRequest>,
    /// Cancellation token for graceful shutdown.
    cancel: CancellationToken,
    /// The engine sync state.
    sync_state: EngineSyncState,
}

impl<E: DirectEngineApi> fmt::Debug for DirectEngineProcessor<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectEngineProcessor").field("sync_state", &self.sync_state).finish()
    }
}

impl<E: DirectEngineApi> DirectEngineProcessor<E> {
    /// Creates a new processor.
    pub fn new(
        client: Arc<E>,
        rx: mpsc::Receiver<EngineActorRequest>,
        cancel: CancellationToken,
    ) -> Self {
        Self { client, rx, cancel, sync_state: EngineSyncState::new() }
    }

    /// Creates a new processor with an existing sync state.
    pub const fn with_sync_state(
        client: Arc<E>,
        rx: mpsc::Receiver<EngineActorRequest>,
        cancel: CancellationToken,
        sync_state: EngineSyncState,
    ) -> Self {
        Self { client, rx, cancel, sync_state }
    }

    /// Returns a reference to the sync state.
    pub const fn sync_state(&self) -> &EngineSyncState {
        &self.sync_state
    }

    /// Runs the request processing loop.
    pub async fn run(mut self) -> Result<(), EngineActorError> {
        debug!("Starting engine processor");

        loop {
            tokio::select! {
                () = self.cancel.cancelled() => {
                    debug!("Processor cancelled");
                    return Err(EngineActorError::Cancelled);
                }
                request = self.rx.recv() => {
                    match request {
                        Some(req) => self.handle_request(req).await,
                        None => {
                            debug!("Request channel closed");
                            return Err(EngineActorError::ChannelClosed);
                        }
                    }
                }
            }
        }
    }

    /// Handles a single request.
    async fn handle_request(&self, request: EngineActorRequest) {
        trace!("Processing request");

        match request {
            EngineActorRequest::Build(req) => {
                let result = BuildHandler::handle(
                    self.client.as_ref(),
                    &self.sync_state,
                    req.parent_hash,
                    req.attributes,
                )
                .await;
                if let Err(e) = &result {
                    warn!("Build request failed: {e}");
                }
                let _ = req.response.send(result);
            }
            EngineActorRequest::Seal(req) => {
                let result =
                    SealHandler::handle(self.client.as_ref(), &self.sync_state, req.payload_id)
                        .await;
                if let Err(e) = &result {
                    warn!("Seal request failed: {e}");
                }
                let _ = req.response.send(result);
            }
            EngineActorRequest::ProcessSafeL2Signal(req) => {
                let result = ConsolidationHandler::handle(
                    self.client.as_ref(),
                    &self.sync_state,
                    req.safe_head,
                )
                .await;
                if let Err(e) = &result {
                    warn!("Consolidation request failed: {e}");
                }
                let _ = req.response.send(result);
            }
            EngineActorRequest::ProcessFinalizedL2BlockNumber(req) => {
                let result = FinalizationHandler::handle(
                    self.client.as_ref(),
                    &self.sync_state,
                    req.finalized_number,
                )
                .await;
                if let Err(e) = &result {
                    warn!("Finalization request failed: {e}");
                }
                let _ = req.response.send(result);
            }
            EngineActorRequest::ProcessUnsafeL2Block(req) => {
                let result = UnsafeBlockHandler::handle(
                    self.client.as_ref(),
                    &self.sync_state,
                    req.envelope,
                )
                .await;
                if let Err(e) = &result {
                    warn!("Unsafe block request failed: {e}");
                }
                let _ = req.response.send(result);
            }
            EngineActorRequest::Reset(req) => {
                let result = ResetHandler::handle(self.client.as_ref(), &self.sync_state).await;
                if let Err(e) = &result {
                    error!("Reset request failed: {e}");
                }
                let _ = req.response.send(result);
            }
        }
    }
}
