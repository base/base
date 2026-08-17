//! Simplex consensus actor.
//!
//! Phase 1 skeleton: this wires the [`NodeActor`] request/response plumbing and
//! the read-side status [`watch`] channel, but carries **no consensus logic** —
//! the commonware simplex engine is integrated in Phase 2. Requests that would
//! require the engine return [`SimplexError::NotImplemented`].

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::{ConsensusStatus, SimplexError, SimplexRequest};
use crate::NodeActor;

/// Operating mode for the simplex consensus actor.
///
/// Drives the staged rollout. Defaults to [`Off`](SimplexMode::Off). In
/// `Off`/`Passive`/`Shadow` the actor is **not** authoritative for block
/// production and must never take the node down (see the isolation invariant on
/// [`SimplexActor::start`]); only in `Active`/`Primary` is it authoritative.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SimplexMode {
    /// Actor not participating; no side effects. Shipped default.
    #[default]
    Off,
    /// Joins the consensus mesh and runs views, but its outputs feed nobody.
    Passive,
    /// As `Passive`, plus compares decisions against op-conductor for divergence.
    Shadow,
    /// Authoritative for block production on this node.
    Active,
    /// Authoritative fleet-wide; op-conductor is a monitored standby.
    Primary,
}

impl SimplexMode {
    /// Returns whether the actor is authoritative for block production in this
    /// mode. Only `Active`/`Primary` are authoritative; a fatal error may
    /// propagate only in those modes.
    pub const fn is_authoritative(self) -> bool {
        matches!(self, Self::Active | Self::Primary)
    }
}

/// Actor that owns the simplex consensus engine and drives it from a
/// `tokio::select!` loop.
///
/// Phase 1 is a no-op skeleton; the commonware `Engine` and its `Handle`, plus
/// the dedicated `commonware-p2p` transport, are added in Phase 2.
#[derive(Debug)]
pub struct SimplexActor {
    mode: SimplexMode,
    request_rx: mpsc::Receiver<SimplexRequest>,
    status_tx: watch::Sender<ConsensusStatus>,
}

impl SimplexActor {
    /// Creates a new simplex actor.
    pub const fn new(
        mode: SimplexMode,
        request_rx: mpsc::Receiver<SimplexRequest>,
        status_tx: watch::Sender<ConsensusStatus>,
    ) -> Self {
        Self { mode, request_rx, status_tx }
    }

    /// Handles a single request against the Phase 1 skeleton.
    ///
    /// No consensus logic exists yet, so every request resolves to
    /// [`SimplexError::NotImplemented`]. Responder drops are logged, not fatal.
    fn handle_request(&self, request: SimplexRequest) {
        match request {
            SimplexRequest::Propose { response_tx, .. } => {
                if response_tx.send(Err(SimplexError::NotImplemented)).is_err() {
                    debug!(target: "simplex", "propose response receiver dropped");
                }
            }
            SimplexRequest::IsLeader { response_tx } => {
                if response_tx.send(Err(SimplexError::NotImplemented)).is_err() {
                    debug!(target: "simplex", "is_leader response receiver dropped");
                }
            }
        }
    }
}

#[async_trait]
impl NodeActor for SimplexActor {
    type Error = SimplexError;
    type StartData = CancellationToken;

    /// Runs the no-op select loop until cancellation or request-channel closure.
    ///
    /// **Isolation is owned by the framework, not this actor.** The simplex actor
    /// is spawned in the `non_fatal` group of `spawn_and_wait!` (`service/util.rs`),
    /// which installs **no** drop guard and swallows any error — so this actor may
    /// return `Ok` or `Err` freely without cancelling the shared token or
    /// affecting the live op-conductor path. That lets the loop be a
    /// straightforward `CheckpointActor`-style select (inlined directly under the
    /// trait impl, matching `CheckpointActor::start`): return on cancellation, and
    /// return on request-channel closure (which happens in Phase 1 when no consumer
    /// holds the client). No per-actor idle-until-cancel workaround is needed.
    async fn start(mut self, cancellation: Self::StartData) -> Result<(), Self::Error> {
        // Re-publish the initial status on startup. In Phase 1 this is the only
        // write to the channel (there is no engine to drive updates yet); Phase 2
        // replaces it with live notarized/finalized/leadership updates from the
        // commonware `Reporter`.
        self.status_tx.send_replace(ConsensusStatus::default());

        loop {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    info!(target: "simplex", mode = ?self.mode, "simplex actor received shutdown signal");
                    return Ok(());
                }
                maybe_request = self.request_rx.recv() => {
                    let Some(request) = maybe_request else {
                        debug!(target: "simplex", mode = ?self.mode, "simplex request channel closed; exiting actor (non-fatal)");
                        return Ok(());
                    };
                    self.handle_request(request);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::actors::simplex::{ConsensusProposer, ConsensusStatusReader, SimplexClient};

    fn test_envelope() -> BaseExecutionPayloadEnvelope {
        BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash: B256::ZERO,
                fee_recipient: alloy_primitives::Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: alloy_primitives::Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number: 0,
                gas_limit: 0,
                gas_used: 0,
                timestamp: 0,
                extra_data: Default::default(),
                base_fee_per_gas: Default::default(),
                block_hash: B256::ZERO,
                transactions: vec![],
            }),
        }
    }

    fn actor_with_client(mode: SimplexMode) -> (SimplexActor, SimplexClient) {
        let (request_tx, request_rx) = mpsc::channel(8);
        let (status_tx, status_rx) = watch::channel(ConsensusStatus::default());
        let actor = SimplexActor::new(mode, request_rx, status_tx);
        let client = SimplexClient::new(request_tx, status_rx);
        (actor, client)
    }

    #[test]
    fn only_active_and_primary_are_authoritative() {
        assert!(!SimplexMode::Off.is_authoritative());
        assert!(!SimplexMode::Passive.is_authoritative());
        assert!(!SimplexMode::Shadow.is_authoritative());
        assert!(SimplexMode::Active.is_authoritative());
        assert!(SimplexMode::Primary.is_authoritative());
    }

    #[tokio::test]
    async fn cancellation_shuts_down_cleanly() {
        let (actor, _client) = actor_with_client(SimplexMode::Off);
        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(actor.start(cancellation.clone()));
        cancellation.cancel();
        assert!(handle.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn skeleton_requests_return_not_implemented() {
        let (actor, client) = actor_with_client(SimplexMode::Off);
        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(actor.start(cancellation.clone()));

        let propose_err = client.propose(test_envelope()).await.unwrap_err();
        assert!(matches!(propose_err, SimplexError::NotImplemented));

        let leader_err = client.is_leader().await.unwrap_err();
        assert!(matches!(leader_err, SimplexError::NotImplemented));

        cancellation.cancel();
        assert!(handle.await.unwrap().is_ok());
    }

    /// The actor returns `Ok` on request-channel closure (which happens in Phase
    /// 1 when no consumer holds the client) without cancelling the shared token.
    /// Node isolation is now the framework's responsibility: the actor is spawned
    /// in the `non_fatal` group of `spawn_and_wait!`, which swallows this return —
    /// so the actor itself no longer needs to idle-until-cancel.
    #[tokio::test]
    async fn channel_close_returns_ok_without_cancelling() {
        let (actor, client) = actor_with_client(SimplexMode::Passive);
        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(actor.start(cancellation.clone()));

        drop(client);
        assert!(handle.await.unwrap().is_ok(), "actor returns Ok on channel close");
        assert!(
            !cancellation.is_cancelled(),
            "actor must not cancel the shared token itself; isolation is the framework's job"
        );
    }
}
