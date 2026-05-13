use std::sync::Arc;

use base_common_genesis::RollupConfig;
use base_consensus_engine::{EngineClient, EngineState};
use derive_more::Constructor;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{EngineError, EngineRpcRequest, NodeActor};

/// Processor for [`EngineRpcRequest`] requests.
#[derive(Constructor, Debug)]
pub struct EngineRpcProcessor<EngineClient_: EngineClient> {
    /// An [`EngineClient`] used for creating engine tasks.
    engine_client: Arc<EngineClient_>,
    /// The [`RollupConfig`] used to build tasks.
    rollup_config: Arc<RollupConfig>,
    /// Receiver for [`EngineState`] updates.
    engine_state_receiver: watch::Receiver<EngineState>,
    /// Receiver for engine queue length updates.
    engine_queue_length_receiver: watch::Receiver<usize>,
}

impl<EngineClient_> EngineRpcProcessor<EngineClient_>
where
    EngineClient_: EngineClient + 'static,
{
    async fn run(
        self,
        cancellation: CancellationToken,
        mut request_channel: mpsc::Receiver<EngineRpcRequest>,
    ) -> Result<(), EngineError> {
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_ENGINE_RPC_QUERIES));
        let this = Arc::new(self);

        loop {
            let query = tokio::select! {
                _ = cancellation.cancelled() => {
                    warn!(target: "engine", "EngineRpcProcessor received shutdown signal");
                    return Ok(());
                }
                query = request_channel.recv() => {
                    let Some(query) = query else {
                        error!(target: "engine", "Engine rpc request receiver closed unexpectedly");
                        return Err(EngineError::ChannelClosed);
                    };
                    query
                }
            };

            let permit = tokio::select! {
                _ = cancellation.cancelled() => {
                    warn!(target: "engine", "EngineRpcProcessor received shutdown signal");
                    return Ok(());
                }
                permit = Arc::clone(&semaphore).acquire_owned() => {
                    permit.expect("semaphore is never closed")
                }
            };

            let handler = Arc::clone(&this);
            // Spawned sub-tasks are intentionally detached. On shutdown, in-flight
            // sub-tasks may still be running. This is acceptable because each
            // request sends its response through a oneshot channel that the caller
            // has likely already dropped, so the worst case is wasted work.
            tokio::spawn(async move {
                if let Err(e) = handler.handle_rpc_request(query).await {
                    error!(target: "engine", error = %e, "engine rpc request failed");
                }
                drop(permit);
            });
        }
    }

    async fn handle_rpc_request(&self, request: EngineRpcRequest) -> Result<(), EngineError> {
        match request {
            EngineRpcRequest::EngineQuery(req) => {
                trace!(target: "engine", ?req, "Received engine query.");

                if let Err(e) = req
                    .handle(
                        &self.engine_state_receiver,
                        &self.engine_queue_length_receiver,
                        &self.engine_client,
                        &self.rollup_config,
                    )
                    .await
                {
                    warn!(target: "engine", err = ?e, "Failed to handle engine query.");
                }
            }
        }

        Ok(())
    }
}

/// Maximum number of engine RPC queries processed concurrently.
/// Bounds concurrent requests to avoid overwhelming the execution engine.
const MAX_CONCURRENT_ENGINE_RPC_QUERIES: usize = 16;

#[async_trait::async_trait]
impl<EngineClient_> NodeActor for EngineRpcProcessor<EngineClient_>
where
    EngineClient_: EngineClient + 'static,
{
    type Error = EngineError;
    type StartData = (CancellationToken, mpsc::Receiver<EngineRpcRequest>);

    async fn start(
        self,
        (cancellation, request_channel): Self::StartData,
    ) -> Result<(), Self::Error> {
        self.run(cancellation, request_channel).await
    }
}
