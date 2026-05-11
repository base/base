//! The [`EngineActor`].

use async_trait::async_trait;
use derive_more::Constructor;
use futures::FutureExt;
use tokio::sync::mpsc;
use tokio_util::{
    future::FutureExt as _,
    sync::{CancellationToken, WaitForCancellationFuture},
};

use crate::{
    EngineActorRequest, EngineError, EngineProcessingRequest, EngineRequestReceiver, NodeActor,
    actors::CancellableContext,
};

/// The [`EngineActor`] is an intermediary that receives [`EngineActorRequest`] and delegates:
/// - Node engine requests to the configured [`EngineRequestReceiver`].
#[derive(Constructor, Debug)]
pub struct EngineActor<EngineRequestReceiver_>
where
    EngineRequestReceiver_: EngineRequestReceiver,
{
    /// The cancellation token shared by all tasks.
    cancellation_token: CancellationToken,
    /// The inbound request channel.
    inbound_request_rx: mpsc::Receiver<EngineActorRequest>,
    /// The processor for engine requests
    engine_receiver: EngineRequestReceiver_,
}

impl<EngineRequestReceiver_> CancellableContext for EngineActor<EngineRequestReceiver_>
where
    EngineRequestReceiver_: EngineRequestReceiver,
{
    fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.cancellation_token.cancelled()
    }
}

#[async_trait]
impl<EngineRequestReceiver_> NodeActor for EngineActor<EngineRequestReceiver_>
where
    EngineRequestReceiver_: EngineRequestReceiver + 'static,
{
    type Error = EngineError;
    type StartData = ();

    async fn start(mut self, _: Self::StartData) -> Result<(), Self::Error> {
        let (engine_processing_tx, engine_processing_rx) = mpsc::channel(1024);

        // Helper to DRY task completion handling.
        let handle_task_result = |task_name: &'static str, cancel_token: CancellationToken| {
            move |result: Option<Result<Result<(), EngineError>, tokio::task::JoinError>>| async move {
                cancel_token.cancel();

                let Some(result) = result else {
                    warn!(target: "engine", task_name, "Task cancelled");
                    return Ok(());
                };

                let Ok(result) = result else {
                    error!(target: "engine", result = ?result, task_name, "Task panicked");
                    return Err(EngineError::ChannelClosed);
                };

                match result {
                    Ok(()) => {
                        info!(target: "engine", task_name, "Task completed successfully");
                        Ok(())
                    }
                    Err(err) => {
                        error!(target: "engine", error = ?err, task_name, "Task failed");
                        Err(err)
                    }
                }
            }
        };

        let processing_cancellation = self.cancellation_token.clone();
        // Start the engine processing task.
        let processing_handle = self
            .engine_receiver
            .start(engine_processing_rx)
            .with_cancellation_token(&processing_cancellation)
            .then(handle_task_result("Engine processing", processing_cancellation.clone()));

        // Helper to send processing requests with error handling.
        let send_engine_processing_request = |req: EngineProcessingRequest| async {
            engine_processing_tx.send(req).await.map_err(|_| {
                error!(target: "engine", "Engine processing channel closed unexpectedly");
                self.cancellation_token.clone().cancel();
                EngineError::ChannelClosed
            })
        };

        loop {
            tokio::select! {
                _ = self.cancellation_token.cancelled() => {
                    warn!(target: "engine", "EngineActor received shutdown signal. Awaiting task completion.");

                    processing_handle.await?;

                    return Ok(());
                }

                req = self.inbound_request_rx.recv() => {
                    let Some(request) = req else {
                        error!(target: "engine", "Engine inbound request receiver closed unexpectedly");
                        self.cancellation_token.cancel();
                        return Err(EngineError::ChannelClosed);
                    };

                    // Route the request to the appropriate channel.
                    match request {
                        EngineActorRequest::BuildRequest(build_req) => {
                            send_engine_processing_request(EngineProcessingRequest::Build(build_req)).await?;
                        }
                        EngineActorRequest::ProcessSafeL2SignalRequest(signal) => {
                            send_engine_processing_request(EngineProcessingRequest::ProcessSafeL2Signal(signal)).await?;
                        }
                        EngineActorRequest::ProcessDelegatedForkchoiceUpdateRequest(update) => {
                            send_engine_processing_request(EngineProcessingRequest::ProcessDelegatedForkchoiceUpdate(update)).await?;
                        }
                        EngineActorRequest::ProcessFinalizedL2BlockNumberRequest(block_number) => {
                            send_engine_processing_request(EngineProcessingRequest::ProcessFinalizedL2BlockNumber(block_number)).await?;
                        }
                        EngineActorRequest::ProcessUnsafeL2BlockRequest(envelope) => {
                            send_engine_processing_request(EngineProcessingRequest::ProcessUnsafeL2Block(envelope)).await?;
                        }
                        EngineActorRequest::ProcessLocalUnsafeL2BlockRequest(envelope) => {
                            send_engine_processing_request(EngineProcessingRequest::ProcessLocalUnsafeL2Block(envelope)).await?;
                        }
                        EngineActorRequest::ResetRequest(reset_req) => {
                            send_engine_processing_request(EngineProcessingRequest::Reset(reset_req)).await?;
                        }
                        EngineActorRequest::GetPayloadRequest(get_payload_req) => {
                            send_engine_processing_request(EngineProcessingRequest::GetPayload(get_payload_req)).await?;
                        }
                        EngineActorRequest::SealRequest(seal_req) => {
                            send_engine_processing_request(EngineProcessingRequest::Seal(seal_req)).await?;
                        }
                    }
                }
            }
        }
    }
}
