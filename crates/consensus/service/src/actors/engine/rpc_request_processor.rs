use std::sync::Arc;

use base_consensus_engine::{EngineClient, EngineState};
use base_consensus_genesis::RollupConfig;
use derive_more::Constructor;
use tokio::{
    sync::{Semaphore, mpsc, watch},
    task::JoinHandle,
};

use crate::{EngineError, EngineRpcRequest};

/// Requires that the implementor handles [`EngineRpcRequest`]s via the provided channel.
/// Note: this exists to facilitate unit testing rather than consolidate multiple implementations
/// under a well-thought-out interface.
pub trait EngineRpcRequestReceiver: Send + Sync {
    /// Starts a task to handle engine queries.
    ///
    /// Requests are processed concurrently and may complete out-of-order.
    /// A bounded semaphore limits the number of in-flight requests.
    fn start(
        self,
        request_channel: mpsc::Receiver<EngineRpcRequest>,
    ) -> JoinHandle<Result<(), EngineError>>;
}

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

impl<EngineClient_> EngineRpcRequestReceiver for EngineRpcProcessor<EngineClient_>
where
    EngineClient_: EngineClient + 'static,
{
    fn start(
        self,
        mut request_channel: mpsc::Receiver<EngineRpcRequest>,
    ) -> JoinHandle<Result<(), EngineError>> {
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_ENGINE_RPC_QUERIES));
        let this = Arc::new(self);
        tokio::spawn(async move {
            loop {
                let Some(query) = request_channel.recv().await else {
                    error!(target: "engine", "Engine rpc request receiver closed unexpectedly");
                    return Err(EngineError::ChannelClosed);
                };
                let permit = Arc::clone(&semaphore)
                    .acquire_owned()
                    .await
                    .expect("semaphore is never closed");
                let handler = Arc::clone(&this);
                // Spawned sub-tasks are intentionally detached. On shutdown, when the
                // request channel closes, this loop exits but in-flight sub-tasks may
                // still be running. This is acceptable because each request sends its
                // response through a oneshot channel that the caller has likely already
                // dropped, so the worst case is wasted work — not incorrect behavior.
                tokio::spawn(async move {
                    if let Err(e) = handler.handle_rpc_request(query).await {
                        error!(target: "engine", error = %e, "engine rpc request failed");
                    }
                    drop(permit);
                });
            }
        })
    }
}
