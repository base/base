//! Engine request handling across the sync-on-startup transition.

use std::sync::Arc;

use base_consensus_engine::{EngineClient, Metrics as EngineMetrics};
use base_protocol::L2BlockInfo;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

use crate::{
    Conductor, DisabledEngineDerivationClient, EngineActorRequest, EngineError,
    EngineRequestReceiver, NodeOperatingMode, SequencerEngineRequestCoordinator,
    ValidatorEngineRequestHandler,
};

/// Asks the engine to leave the sync phase. The engine replies with the unsafe head it hands to
/// the sequencer, or [`None`] while EL sync and the initial derivation reset are incomplete.
pub type StartupSyncHandoffRequest = oneshot::Sender<Option<L2BlockInfo>>;

/// Routes engine requests like a validator until handoff, then like a node started directly in
/// the target sequencer mode.
///
/// The validator phase seeds engine state from reth without a forkchoice update and lets gossip
/// `InsertTask`s drive EL sync, so reth can reorg off a deep local fork. Handoff happens between
/// requests, so the head returned to the caller is exactly the head the sequencer inherits.
#[derive(Debug)]
pub struct StartupSyncEngineRequestHandler<EngineClient_>
where
    EngineClient_: EngineClient + 'static,
{
    /// Handler used until handoff.
    pub validator: ValidatorEngineRequestHandler<EngineClient_>,
    /// Mode the node switches to after handoff.
    pub target: NodeOperatingMode,
    /// Conductor used by the post-handoff coordinator to resolve its bootstrap role.
    pub conductor: Option<Arc<dyn Conductor>>,
    /// Unsafe-head channel read by the sequencer engine client.
    pub unsafe_head_tx: watch::Sender<L2BlockInfo>,
    /// Handoff requests from the syncing sequencer.
    pub handoff_rx: mpsc::Receiver<StartupSyncHandoffRequest>,
}

impl<EngineClient_> StartupSyncEngineRequestHandler<EngineClient_>
where
    EngineClient_: EngineClient + 'static,
{
    /// Runs the validator phase until a handoff request finds the engine ready, returning the
    /// reply channel for that request.
    async fn run_validator_phase(
        &mut self,
        request_channel: &mut mpsc::Receiver<EngineActorRequest>,
    ) -> Result<StartupSyncHandoffRequest, EngineError> {
        self.validator.bootstrap().await;
        loop {
            let _iter_timer =
                base_metrics::timed!(EngineMetrics::engine_processor_iteration_duration());
            self.validator.drain().await?;

            tokio::select! {
                biased;
                Some(reply) = self.handoff_rx.recv() => {
                    if self.validator.processor().el_sync_complete() {
                        return Ok(reply);
                    }
                    if reply.send(None).is_err() {
                        warn!(target: "engine", "sync-on-startup handoff requester dropped");
                    }
                }
                request = request_channel.recv() => {
                    let Some(request) = request else {
                        error!(target: "engine", "Engine processing request receiver closed unexpectedly");
                        return Err(EngineError::ChannelClosed);
                    };
                    self.validator.handle_request(request).await?;
                }
            }
        }
    }
}

impl<EngineClient_> EngineRequestReceiver for StartupSyncEngineRequestHandler<EngineClient_>
where
    EngineClient_: EngineClient + 'static,
{
    fn start(
        mut self,
        mut request_channel: mpsc::Receiver<EngineActorRequest>,
    ) -> JoinHandle<Result<(), EngineError>> {
        tokio::spawn(async move {
            let reply = self.run_validator_phase(&mut request_channel).await?;

            let isolated = self.target.is_isolated();
            let mut processor = self.validator.into_processor();
            if isolated {
                processor.set_derivation_client(Box::new(DisabledEngineDerivationClient));
            }
            let head = processor.engine_state().sync_state.unsafe_head();
            self.unsafe_head_tx.send_replace(head);

            let mut coordinator = SequencerEngineRequestCoordinator::new(
                processor,
                self.target.is_shadow_sequencer(),
                self.conductor,
                false,
                self.unsafe_head_tx,
            );
            coordinator.resume_after_startup_sync().await;
            if isolated {
                coordinator.close_canonical_ingress();
            }
            if reply.send(Some(head)).is_err() {
                warn!(target: "engine", "sync-on-startup handoff requester dropped");
            }

            coordinator.run(request_channel).await
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumberOrTag;
    use base_common_genesis::RollupConfig;
    use base_consensus_engine::{
        Engine, EngineState,
        test_utils::{test_block_info, test_engine_client_builder},
    };

    use super::*;
    use crate::{EngineProcessor, MockEngineDerivationClient};

    #[tokio::test]
    async fn refuses_handoff_until_el_sync_completes_and_keeps_local_head_unconfirmed() {
        let local_fork_tip = test_block_info(100);
        // No forkchoice response is configured: the sync phase must not FCU the local tip.
        let client = Arc::new(
            test_engine_client_builder()
                .with_block_info_by_tag(BlockNumberOrTag::Latest, local_fork_tip)
                .build(),
        );
        let (state_tx, state_rx) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let processor = EngineProcessor::new(
            client,
            Arc::new(RollupConfig::default()),
            Box::new(MockEngineDerivationClient::new()),
            Engine::new(EngineState::default(), state_tx, queue_tx),
        );
        let (handoff_tx, handoff_rx) = mpsc::channel(1);
        let (unsafe_head_tx, _) = watch::channel(L2BlockInfo::default());
        let (_request_tx, request_rx) = mpsc::channel(8);
        let handle = StartupSyncEngineRequestHandler {
            validator: ValidatorEngineRequestHandler::new(processor),
            target: NodeOperatingMode::IsolatedSequencer,
            conductor: None,
            unsafe_head_tx,
            handoff_rx,
        }
        .start(request_rx);

        let (reply_tx, reply_rx) = oneshot::channel();
        handoff_tx.send(reply_tx).await.unwrap();

        assert_eq!(reply_rx.await.unwrap(), None);
        let state = *state_rx.borrow();
        assert_eq!(state.sync_state.unsafe_head(), local_fork_tip);
        assert!(!state.el_sync_finished);
        handle.abort();
    }
}
