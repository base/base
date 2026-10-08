use alloy_primitives::BlockHash;
use async_trait::async_trait;
use base_consensus_derive::ChainProvider;
use base_consensus_providers::AlloyChainProvider;
use base_protocol::{BlockInfo, L2BlockInfo, SyncStatus};
use thiserror::Error;
use tokio::{
    select,
    sync::{mpsc, watch},
    time,
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use crate::{
    CancellableContext, DerivationActorRequest, DerivationEngineClient, NodeActor,
    actors::derivation::{DerivationDelegateClient, DerivationError},
};

/// The [`NodeActor`] for the delegate derivation sub-routine.
///
/// This actor is responsible for receiving messages from [`NodeActor`]s and polls
/// an external derivation delegation provider for derivation state. It validates
/// the canonicality of the L1 information associated with delegated derivation
/// results against the canonical L1 chain before forwarding updates.
///
/// Once validated, the actor sends the derived safe and finalized L2 info
/// to the [`NodeActor`] responsible for the execution sub-routine.
#[derive(Debug)]
pub struct DelegateDerivationActor<DerivationEngineClient_>
where
    DerivationEngineClient_: DerivationEngineClient,
{
    /// The cancellation token, shared between all tasks.
    cancellation_token: CancellationToken,
    /// The channel on which all inbound requests are received by the [`DelegateDerivationActor`].
    inbound_request_rx: mpsc::Receiver<DerivationActorRequest>,
    /// The Engine client used to interact with the engine.
    engine_client: DerivationEngineClient_,

    /// Derivation delegate provider.
    derivation_delegate_provider: DerivationDelegateClient,
    /// L1 provider for validating L1 info for derivation delegation.
    l1_provider: AlloyChainProvider,
    /// Publishes the delegate-reported L1 derivation cursor, once the engine safe head reaches
    /// the delegate's safe head.
    derivation_origin_tx: watch::Sender<Option<BlockInfo>>,

    /// The engine's L2 safe head, according to updates from the Engine.
    engine_l2_safe_head: L2BlockInfo,
    /// The last delegate status whose `current_l1` is not published yet, because the engine
    /// has not reached its safe head.
    unapplied_status: Option<SyncStatus>,
    /// Whether the engine sync has completed. This will only ever go from false -> true.
    has_engine_sync_completed: bool,
}

impl<DerivationEngineClient_> CancellableContext
    for DelegateDerivationActor<DerivationEngineClient_>
where
    DerivationEngineClient_: DerivationEngineClient,
{
    fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.cancellation_token.cancelled()
    }
}

impl<DerivationEngineClient_> DelegateDerivationActor<DerivationEngineClient_>
where
    DerivationEngineClient_: DerivationEngineClient,
{
    /// Creates a new instance of the [`DelegateDerivationActor`].
    pub fn new(
        engine_client: DerivationEngineClient_,
        cancellation_token: CancellationToken,
        inbound_request_rx: mpsc::Receiver<DerivationActorRequest>,
        derivation_delegate_provider: DerivationDelegateClient,
        l1_provider: AlloyChainProvider,
        derivation_origin_tx: watch::Sender<Option<BlockInfo>>,
    ) -> Self {
        Self {
            cancellation_token,
            inbound_request_rx,
            engine_client,
            derivation_delegate_provider,
            l1_provider,
            derivation_origin_tx,
            engine_l2_safe_head: L2BlockInfo::default(),
            unapplied_status: None,
            has_engine_sync_completed: false,
        }
    }
}

#[async_trait]
impl<DerivationEngineClient_> NodeActor for DelegateDerivationActor<DerivationEngineClient_>
where
    DerivationEngineClient_: DerivationEngineClient + 'static,
{
    type Error = DerivationError;
    type StartData = ();

    async fn start(mut self, _: Self::StartData) -> Result<(), Self::Error> {
        self.start_delegate_derivation().await
    }
}

impl<DerivationEngineClient_> DelegateDerivationActor<DerivationEngineClient_>
where
    DerivationEngineClient_: DerivationEngineClient + 'static,
{
    /// Hardcoded poll interval for Derivation Delegation
    const DERIVATION_DELEGATE_POLL_INTERVAL: std::time::Duration =
        std::time::Duration::from_secs(4);

    /// Validates a single L1 block height and hash against the canonical L1 chain.
    async fn validate_l1_block(
        &mut self,
        context: &str,
        l1_block_number: u64,
        expected_hash: BlockHash,
    ) -> Result<(), DerivationDelegationError> {
        let block = self
            .l1_provider
            .block_info_by_number(l1_block_number)
            .await
            .map_err(|e| DerivationDelegationError::L1Provider(e.to_string()))?;

        if block.hash != expected_hash {
            return Err(DerivationDelegationError::L1ValidationFailed {
                context: context.to_string(),
                number: l1_block_number,
                expected: expected_hash,
                actual: block.hash,
            });
        }

        Ok(())
    }

    /// Verifies that the L1 info reported by the derivation delegate
    /// are consistent with canonical L1 chain.
    async fn validate_sync_status(&mut self, v: &SyncStatus) -> bool {
        let checks = [
            ("L1 Origin of Safe L2", v.safe_l2.l1_origin.number, v.safe_l2.l1_origin.hash),
            (
                "L1 Origin of Finalized L2",
                v.finalized_l2.l1_origin.number,
                v.finalized_l2.l1_origin.hash,
            ),
            ("Current L1", v.current_l1.number, v.current_l1.hash),
        ];
        for (context, number, hash) in checks {
            if let Err(err) = self.validate_l1_block(context, number, hash).await {
                warn!(
                    target: "derivation",
                    context = context,
                    error = %err,
                    "L1 inconsistency detected at sync status from delegate"
                );
                return false;
            }
        }
        true
    }

    /// Fetches, validates, and applies sync status from the derivation delegate.
    async fn fetch_and_apply_delegate_safe_head(&mut self) -> Result<(), DerivationError> {
        let sync_status = match self.derivation_delegate_provider.fetch_sync_status().await {
            Ok(status) => status,
            Err(_) => {
                warn!(target: "derivation", "Failed to fetch sync status from delegate");
                return Ok(());
            }
        };

        if !self.validate_sync_status(&sync_status).await {
            // Validation failures here are expected to be transient, so we skip processing
            // this sync status and continue delegating derivation instead of treating it as
            // fatal.
            return Ok(());
        }

        self.forward_delegate_status(sync_status).await
    }

    /// Sends the safe and finalized L2 heads of a validated delegate status to the engine.
    ///
    /// The delegate's `current_l1` is published once the engine safe head reaches the status's
    /// safe head, as local derivation does, so `optimism_syncStatus` never reports a
    /// `current_l1` ahead of the safe heads derived before it.
    async fn forward_delegate_status(
        &mut self,
        sync_status: SyncStatus,
    ) -> Result<(), DerivationError> {
        self.engine_client
            .send_safe_l2_signal(sync_status.safe_l2.into())
            .await
            .map_err(|e| DerivationError::Sender(Box::new(e)))?;

        self.engine_client
            .send_finalized_l2_block(sync_status.finalized_l2.block_info.number)
            .await
            .map_err(|e| DerivationError::Sender(Box::new(e)))?;

        debug!(
            target: "derivation",
            safe_l2 = ?sync_status.safe_l2,
            finalized_l2 = ?sync_status.finalized_l2,
            "Processed sync status from delegate"
        );

        self.unapplied_status = Some(sync_status);
        self.publish_origin_if_applied();
        Ok(())
    }

    /// Publishes the `current_l1` of the unapplied delegate status once the engine safe head
    /// has reached its safe head.
    fn publish_origin_if_applied(&mut self) {
        let Some(status) = &self.unapplied_status else {
            return;
        };
        if self.engine_l2_safe_head.block_info.number >= status.safe_l2.block_info.number {
            self.derivation_origin_tx.send_replace(Some(status.current_l1));
            self.unapplied_status = None;
        }
    }

    async fn start_delegate_derivation(mut self) -> Result<(), DerivationError> {
        info!(target: "derivation", "Starting derivation with delegation");
        let mut delegated_derivation_ticker =
            time::interval(Self::DERIVATION_DELEGATE_POLL_INTERVAL);
        delegated_derivation_ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        loop {
            select! {
                biased;

                _ = self.cancellation_token.cancelled() => {
                    info!(
                        target: "derivation",
                        "Received shutdown signal. Exiting derivation task."
                    );
                    return Ok(());
                }
                req = self.inbound_request_rx.recv() => {
                    let Some(request_type) = req else {
                        error!(target: "derivation", "DerivationActor inbound request receiver closed unexpectedly");
                        self.cancellation_token.cancel();
                        return Err(DerivationError::RequestReceiveFailed);
                    };

                    self.handle_derivation_delegation_actor_request(request_type).await?;
                }
                _ = delegated_derivation_ticker.tick(),
                if self.has_engine_sync_completed => {
                    self.fetch_and_apply_delegate_safe_head().await?;
                }
            }
        }
    }

    async fn handle_derivation_delegation_actor_request(
        &mut self,
        request_type: DerivationActorRequest,
    ) -> Result<(), DerivationError> {
        match request_type {
            DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(safe_head) => {
                debug!(target: "derivation", safe_head = ?*safe_head, "Received safe head from engine.");
                self.engine_l2_safe_head = *safe_head;
                self.publish_origin_if_applied();
            }
            DerivationActorRequest::ProcessEngineSyncCompletionRequest(safe_head) => {
                info!(target: "derivation", "Engine finished syncing, starting derivation.");
                self.engine_l2_safe_head = *safe_head;
                self.has_engine_sync_completed = true;
            }
            DerivationActorRequest::ProcessEngineSignalRequest(_)
            | DerivationActorRequest::ProcessFinalizedL1Block(_)
            | DerivationActorRequest::ProcessL1HeadUpdateRequest(_) => {
                debug!(target: "derivation", request_type = ?request_type, "Ignoring request while derivation delegation");
            }
            #[cfg(test)]
            DerivationActorRequest::CurrentStateRequest(result_tx) => {
                let state = if self.has_engine_sync_completed {
                    crate::DerivationState::Deriving
                } else {
                    crate::DerivationState::AwaitingELSyncCompletion
                };
                let _ = result_tx.send(state);
            }
        }
        Ok(())
    }
}

#[derive(Error, Debug)]
enum DerivationDelegationError {
    /// The L1 provider returned an error (network, RPC, etc.)
    #[error("L1 provider error: {0}")]
    L1Provider(String),

    /// The hash provided by the derivation delegation does not match the canonical chain.
    #[error("L1 inconsistency in {context} at block {number}: expected {expected}, got {actual}")]
    L1ValidationFailed { context: String, number: u64, expected: BlockHash, actual: BlockHash },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::derivation::engine_client::MockDerivationEngineClient;

    /// An L2 head numbered `number`.
    fn l2_head(number: u64) -> L2BlockInfo {
        L2BlockInfo { block_info: BlockInfo { number, ..Default::default() }, ..Default::default() }
    }

    /// The delegate's `current_l1` is published once the engine safe head reaches the delegate's
    /// safe head: after the engine reports it, or at once when the engine already holds it.
    #[tokio::test]
    async fn publishes_the_delegate_current_l1_once_the_engine_reaches_its_safe_head() {
        let mut engine_client = MockDerivationEngineClient::new();
        engine_client.expect_send_safe_l2_signal().returning(|_| Ok(()));
        engine_client.expect_send_finalized_l2_block().returning(|_| Ok(()));
        let (derivation_origin_tx, derivation_origin) = watch::channel(None);
        let (_inbound_request_tx, inbound_request_rx) = mpsc::channel(1);
        let mut actor = DelegateDerivationActor::new(
            engine_client,
            CancellationToken::new(),
            inbound_request_rx,
            DerivationDelegateClient::new("http://127.0.0.1:1".parse().unwrap()).unwrap(),
            AlloyChainProvider::new_http("http://127.0.0.1:1".parse().unwrap(), 1),
            derivation_origin_tx,
        );
        let current_l1 = BlockInfo { number: 50, ..Default::default() };

        actor
            .forward_delegate_status(SyncStatus {
                current_l1,
                safe_l2: l2_head(10),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(*derivation_origin.borrow(), None, "the engine has not applied the safe head");

        let engine_update = |number| {
            DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(l2_head(number)))
        };
        actor.handle_derivation_delegation_actor_request(engine_update(9)).await.unwrap();
        assert_eq!(*derivation_origin.borrow(), None, "the engine is still below the safe head");

        actor.handle_derivation_delegation_actor_request(engine_update(10)).await.unwrap();
        assert_eq!(*derivation_origin.borrow(), Some(current_l1));

        let next_l1 = BlockInfo { number: 51, ..Default::default() };
        actor
            .forward_delegate_status(SyncStatus {
                current_l1: next_l1,
                safe_l2: l2_head(10),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            *derivation_origin.borrow(),
            Some(next_l1),
            "the engine already holds the safe head"
        );
    }
}
