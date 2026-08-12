//! Validator engine request routing.

use std::{sync::Arc, time::Instant};

use alloy_eips::BlockNumberOrTag;
use base_consensus_engine::{
    ConsolidateTask, EngineClient, EngineTask, EngineTaskError, EngineTaskErrors, FinalizeTask,
    Metrics as EngineMetrics, SealTaskError,
};
use opentelemetry::context::FutureExt as OtelFutureExt;
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{error, warn};

use crate::{
    BuildRequest, EngineActorRequest, EngineClientError, EngineDerivationClient, EngineError,
    EngineProcessor, EngineRequestReceiver, GetPayloadRequest, InsertUnsafePayloadRequest, Metrics,
    ReconcileShadowRequest, ResetRequest, ResetRequestOutcome,
};

/// Receives validator engine requests without carrying sequencer configuration.
#[derive(Debug)]
pub struct ValidatorEngineRequestHandler<EngineClient_, DerivationClient>
where
    EngineClient_: EngineClient + 'static,
    DerivationClient: EngineDerivationClient + 'static,
{
    processor: EngineProcessor<EngineClient_, DerivationClient>,
}

impl<EngineClient_, DerivationClient> ValidatorEngineRequestHandler<EngineClient_, DerivationClient>
where
    EngineClient_: EngineClient + 'static,
    DerivationClient: EngineDerivationClient + 'static,
{
    /// Creates a validator request receiver.
    pub const fn new(processor: EngineProcessor<EngineClient_, DerivationClient>) -> Self {
        Self { processor }
    }
}

impl<EngineClient_, DerivationClient> EngineRequestReceiver
    for ValidatorEngineRequestHandler<EngineClient_, DerivationClient>
where
    EngineClient_: EngineClient + 'static,
    DerivationClient: EngineDerivationClient + 'static,
{
    fn start(
        mut self,
        mut request_channel: mpsc::Receiver<EngineActorRequest>,
    ) -> JoinHandle<Result<(), EngineError>> {
        tokio::spawn(async move {
            let reth_head =
                self.processor.client().l2_block_info_by_label(BlockNumberOrTag::Latest).await;
            if let Err(error) = &reth_head {
                warn!(target: "engine", ?error, "Bootstrap: failed to query reth head");
            }
            self.processor.bootstrap_validator(reth_head.ok().flatten()).await;

            loop {
                let _iter_timer =
                    base_metrics::timed!(EngineMetrics::engine_processor_iteration_duration());
                base_metrics::time!(EngineMetrics::engine_processor_drain_duration_seconds(), {
                    self.processor.drain().await.inspect_err(
                        |error| error!(target: "engine", ?error, "Failed to drain engine tasks"),
                    )
                })?;

                let request = base_metrics::time!(
                    EngineMetrics::engine_processor_recv_wait_duration_seconds(),
                    { request_channel.recv().await }
                );
                let Some(request) = request else {
                    error!(target: "engine", "Engine processing request receiver closed unexpectedly");
                    return Err(EngineError::ChannelClosed);
                };

                match request {
                    EngineActorRequest::BuildRequest(request) => {
                        let BuildRequest { attributes, result_tx, otel_cx } = *request;
                        let client = Arc::clone(self.processor.client());
                        let rollup = Arc::clone(self.processor.rollup());
                        let result = self
                            .processor
                            .engine_mut()
                            .build(client, rollup, attributes)
                            .with_context(otel_cx)
                            .await;
                        let error = result
                            .as_ref()
                            .err()
                            .map(|error| (error.severity(), format!("{error:?}")));
                        result_tx.send(result).await.map_err(|_| EngineError::ChannelClosed)?;
                        if let Some((severity, error)) = error {
                            self.processor
                                .handle_engine_task_error_severity(severity, error)
                                .await?;
                        }
                    }
                    EngineActorRequest::GetPayloadRequest(request) => {
                        let GetPayloadRequest { payload_id, attributes, result_tx, otel_cx } =
                            *request;
                        let client = Arc::clone(self.processor.client());
                        let rollup = Arc::clone(self.processor.rollup());
                        let result = self
                            .processor
                            .engine_mut()
                            .get_payload(client, rollup, payload_id, attributes)
                            .with_context(otel_cx)
                            .await;
                        let error = result
                            .as_ref()
                            .err()
                            .map(|error| (error.severity(), format!("{error:?}")));
                        result_tx.send(result).await.map_err(|error| {
                            EngineTaskErrors::Seal(SealTaskError::MpscSend(Box::new(error)))
                        })?;
                        if let Some((severity, error)) = error {
                            self.processor
                                .handle_engine_task_error_severity(severity, error)
                                .await?;
                        }
                    }
                    EngineActorRequest::ProcessSafeL2SignalRequest(safe_signal) => {
                        self.processor.enqueue(EngineTask::Consolidate(Box::new(
                            ConsolidateTask::new(
                                Arc::clone(self.processor.client()),
                                Arc::clone(self.processor.rollup()),
                                safe_signal,
                            ),
                        )));
                    }
                    EngineActorRequest::ProcessFinalizedL2BlockNumberRequest(block_number) => {
                        self.processor.enqueue(EngineTask::Finalize(Box::new(FinalizeTask::new(
                            Arc::clone(self.processor.client()),
                            Arc::clone(self.processor.rollup()),
                            *block_number,
                        ))));
                    }
                    EngineActorRequest::ProcessUnsafeL2BlockRequest(envelope) => {
                        self.processor.handle_external_unsafe_l2_block(*envelope);
                    }
                    EngineActorRequest::ProcessAdminUnsafeL2BlockRequest(envelope) => {
                        self.processor.handle_admin_unsafe_l2_block(*envelope);
                    }
                    EngineActorRequest::ProcessLocalUnsafeL2BlockRequest(request) => {
                        let InsertUnsafePayloadRequest { envelope, result_tx, otel_cx } = *request;
                        let _guard = otel_cx.attach();
                        self.processor.handle_local_unsafe_l2_block(envelope, result_tx);
                    }
                    EngineActorRequest::ReconcileShadowRequest(request) => {
                        let ReconcileShadowRequest { result_tx, .. } = *request;
                        if result_tx
                            .send(Err(EngineClientError::ShadowReconciliationDisabled))
                            .await
                            .is_err()
                        {
                            warn!(target: "engine", "Shadow reconciliation response receiver dropped");
                        }
                    }
                    EngineActorRequest::ResetRequest(request) => {
                        let reset_started = Instant::now();
                        let ResetRequest { result_tx, origin, reason } = *request;
                        let unsafe_before = self.processor.engine_state().sync_state.unsafe_head();
                        if !self.processor.engine_state().el_sync_finished {
                            warn!(target: "engine", "Deferring engine reset: EL sync not yet complete");
                            Metrics::record_engine_reset(
                                origin,
                                reason,
                                ResetRequestOutcome::Deferred,
                                reset_started.elapsed(),
                                unsafe_before,
                                self.processor.engine_state().sync_state.unsafe_head(),
                            );
                            if result_tx.send(Err(EngineClientError::ELSyncing)).await.is_err() {
                                warn!(target: "engine", "Sending ELSyncing response failed");
                            }
                            continue;
                        }

                        warn!(target: "engine", "Received reset request");
                        match self.processor.reset_engine_state().await {
                            Ok(safe_head) => {
                                let response = self
                                    .processor
                                    .notify_derivation_of_reset(safe_head)
                                    .await
                                    .map_err(|error| {
                                        EngineClientError::ResetForkchoiceError(error.to_string())
                                    });
                                let unsafe_after =
                                    self.processor.engine_state().sync_state.unsafe_head();
                                let outcome = if response.is_ok() {
                                    ResetRequestOutcome::from_unsafe_heads(
                                        unsafe_before,
                                        unsafe_after,
                                    )
                                } else {
                                    ResetRequestOutcome::DerivationNotificationFailed
                                };
                                Metrics::record_engine_reset(
                                    origin,
                                    reason,
                                    outcome,
                                    reset_started.elapsed(),
                                    unsafe_before,
                                    unsafe_after,
                                );
                                if result_tx.send(response).await.is_err() {
                                    warn!(target: "engine", "Sending reset response failed");
                                }
                            }
                            Err(error) => {
                                Metrics::record_engine_reset(
                                    origin,
                                    reason,
                                    ResetRequestOutcome::Failed,
                                    reset_started.elapsed(),
                                    unsafe_before,
                                    self.processor.engine_state().sync_state.unsafe_head(),
                                );
                                let response =
                                    Err(EngineClientError::ResetForkchoiceError(error.to_string()));
                                if result_tx.send(response).await.is_err() {
                                    warn!(target: "engine", "Sending reset response failed");
                                    return Err(error);
                                }
                            }
                        }
                    }
                }
            }
        })
    }
}
