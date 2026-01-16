//! Direct engine processor for transport-agnostic engine communication.
//!
//! This module provides [`DirectEngineProcessor`], a port of kona's `EngineProcessor`
//! that is generic over [`DirectEngineApi`] instead of being tied to HTTP transport.
//!
//! # Why This Exists
//!
//! Kona's `EngineProcessor` is tightly coupled to `EngineClient`, which requires HTTP transport.
//! This implementation removes that constraint, allowing the engine processor to work with
//! any transport mechanism (HTTP, in-process, IPC, etc.) via the [`DirectEngineApi`] trait.
//!
//! # Reference
//!
//! See kona's EngineProcessor:
//! <https://github.com/op-rs/kona/blob/24e7e2658e09ac00c8e6cbb48bebe6d10f8fb69d/crates/node/service/src/actors/engine/actor.rs>

use std::{fmt::Debug, sync::Arc};

use async_trait::async_trait;
use base_engine_driver::DirectEngineApi;
use kona_genesis::RollupConfig;
use kona_node_service::{EngineActorRequest, EngineContext, EngineRpcRequest, NodeActor};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    DirectEngineProcessorError, EngineProcessingRequest, RoutedRequest,
    handlers::{
        handle_build_request, handle_derived_attributes, handle_finalized_l1_block,
        handle_reset_request, handle_seal_request, handle_unsafe_block,
    },
};

/// Direct engine processor for transport-agnostic engine communication.
///
/// This is a port of kona's `EngineProcessor` but generic over [`DirectEngineApi`]
/// instead of `EngineClient`, allowing it to work with any transport mechanism.
///
/// # Type Parameters
///
/// * `E` - The engine API implementation, must implement [`DirectEngineApi`]
#[derive(Debug)]
pub struct DirectEngineProcessor<E>
where
    E: DirectEngineApi,
{
    /// The engine driver for direct communication with the execution layer.
    client: Arc<E>,

    /// The rollup configuration.
    rollup: Arc<RollupConfig>,

    /// Receiver for inbound engine actor requests.
    inbound_request_rx: mpsc::Receiver<EngineActorRequest>,

    /// Cancellation token for graceful shutdown.
    cancellation: CancellationToken,
}

impl<E> DirectEngineProcessor<E>
where
    E: DirectEngineApi,
{
    /// Creates a new [`DirectEngineProcessor`].
    pub const fn new(
        client: Arc<E>,
        rollup: Arc<RollupConfig>,
        inbound_request_rx: mpsc::Receiver<EngineActorRequest>,
        cancellation: CancellationToken,
    ) -> Self {
        Self { client, rollup, inbound_request_rx, cancellation }
    }

    /// Returns a reference to the engine client.
    pub const fn client(&self) -> &Arc<E> {
        &self.client
    }

    /// Returns a reference to the rollup configuration.
    pub const fn rollup(&self) -> &Arc<RollupConfig> {
        &self.rollup
    }

    /// Handles an RPC request.
    #[allow(dead_code)]
    async fn handle_rpc_request(&self, request: EngineRpcRequest) {
        debug!(target: "direct_engine", ?request, "Received RPC request");
        // RPC request handling - these are typically debug/admin requests
        // For the initial implementation, we log them
    }

    /// Handles an engine processing request.
    ///
    /// This method routes requests to the appropriate handler based on request type.
    async fn handle_processing_request(
        &self,
        request: EngineProcessingRequest,
    ) -> Result<(), DirectEngineProcessorError> {
        match request {
            EngineProcessingRequest::Build { attributes, result_tx } => {
                handle_build_request(self.client.as_ref(), attributes, result_tx).await
            }
            EngineProcessingRequest::ProcessDerivedL2Attributes(attrs) => {
                handle_derived_attributes(self.client.as_ref(), attrs).await
            }
            EngineProcessingRequest::ProcessFinalizedL1Block(block) => {
                handle_finalized_l1_block(self.client.as_ref(), block).await
            }
            EngineProcessingRequest::ProcessUnsafeL2Block(envelope) => {
                handle_unsafe_block(self.client.as_ref(), envelope).await
            }
            EngineProcessingRequest::Reset { result_tx } => {
                handle_reset_request(self.client.as_ref(), result_tx).await
            }
            EngineProcessingRequest::Seal { payload_id, attributes, result_tx } => {
                handle_seal_request(self.client.as_ref(), payload_id, attributes, result_tx).await
            }
        }
    }
}

#[async_trait]
impl<E> NodeActor for DirectEngineProcessor<E>
where
    E: DirectEngineApi + Debug + 'static,
{
    type Error = DirectEngineProcessorError;
    type StartData = EngineContext;

    async fn start(mut self, context: Self::StartData) -> Result<(), Self::Error> {
        info!(target: "direct_engine", "Starting DirectEngineProcessor");

        let cancellation = context.cancellation.clone();

        // Create channels for internal routing
        let (rpc_tx, mut rpc_rx) = mpsc::channel::<EngineRpcRequest>(1024);
        let (processing_tx, mut processing_rx) = mpsc::channel::<EngineProcessingRequest>(1024);

        // Spawn RPC handling task
        let rpc_client = self.client.clone();
        let rpc_rollup = self.rollup.clone();
        let rpc_cancel = cancellation.clone();

        let rpc_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rpc_cancel.cancelled() => {
                        info!(target: "direct_engine", "RPC handler shutting down");
                        return Ok::<(), DirectEngineProcessorError>(());
                    }
                    request = rpc_rx.recv() => {
                        let Some(request) = request else {
                            warn!(target: "direct_engine", "RPC channel closed");
                            return Err(DirectEngineProcessorError::ChannelClosed);
                        };
                        debug!(
                            target: "direct_engine",
                            ?request,
                            rollup_l2_chain_id = %rpc_rollup.l2_chain_id,
                            "Processing RPC request"
                        );
                        let _ = &rpc_client;
                    }
                }
            }
        });

        // Spawn processing task
        let proc_client = self.client.clone();
        let proc_rollup = self.rollup.clone();
        let proc_cancel = cancellation.clone();

        let processing_handle = tokio::spawn(async move {
            // Create a temporary processor for the spawned task
            let processor = Self {
                client: proc_client,
                rollup: proc_rollup,
                inbound_request_rx: mpsc::channel(1).1, // Dummy receiver
                cancellation: proc_cancel.clone(),
            };

            loop {
                tokio::select! {
                    _ = proc_cancel.cancelled() => {
                        info!(target: "direct_engine", "Processing handler shutting down");
                        return Ok::<(), DirectEngineProcessorError>(());
                    }
                    request = processing_rx.recv() => {
                        let Some(request) = request else {
                            warn!(target: "direct_engine", "Processing channel closed");
                            return Err(DirectEngineProcessorError::ChannelClosed);
                        };
                        debug!(
                            target: "direct_engine",
                            ?request,
                            "Processing engine request"
                        );
                        if let Err(e) = processor.handle_processing_request(request).await {
                            error!(target: "direct_engine", %e, "Failed to process request");
                        }
                    }
                }
            }
        });

        // Notify that sync is complete (for now, we assume EL is ready)
        if context.sync_complete_tx.send(()).is_err() {
            warn!(target: "direct_engine", "Failed to send sync complete signal");
        }

        // Main request routing loop
        loop {
            tokio::select! {
                _ = self.cancellation.cancelled() => {
                    info!(target: "direct_engine", "DirectEngineProcessor received shutdown signal");
                    cancellation.cancel();

                    // Wait for tasks to complete
                    let _ = rpc_handle.await;
                    let _ = processing_handle.await;

                    return Ok(());
                }

                request = self.inbound_request_rx.recv() => {
                    let Some(request) = request else {
                        error!(target: "direct_engine", "Inbound request channel closed");
                        cancellation.cancel();
                        return Err(DirectEngineProcessorError::ChannelClosed);
                    };

                    // Route the request
                    match RoutedRequest::from_actor_request(request) {
                        RoutedRequest::Rpc(rpc_req) => {
                            if rpc_tx.send(rpc_req).await.is_err() {
                                error!(target: "direct_engine", "RPC channel closed");
                                cancellation.cancel();
                                return Err(DirectEngineProcessorError::ChannelClosed);
                            }
                        }
                        RoutedRequest::Processing(processing_req) => {
                            if processing_tx.send(processing_req).await.is_err() {
                                error!(target: "direct_engine", "Processing channel closed");
                                cancellation.cancel();
                                return Err(DirectEngineProcessorError::ChannelClosed);
                            }
                        }
                    }
                }
            }
        }
    }
}
