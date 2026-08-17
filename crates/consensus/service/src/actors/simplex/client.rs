//! Simplex consensus actor client.
//!
//! Consumers interact with the [`SimplexActor`](super::SimplexActor) only through
//! the typed [`SimplexClient`] and the two narrow, mockable traits below —
//! [`ConsensusProposer`] (write side) and [`ConsensusStatusReader`] (read side) —
//! mirroring the `CheckpointWriter` / `ForkchoiceCheckpointReader` split so
//! downstream actors depend on the capability they use, not the concrete client.
//!
//! The read side is a [`watch`] channel carrying a [`ConsensusStatus`] snapshot:
//! leadership and the finalized head are continuously-changing state that the
//! sequencer coordinator samples on its hot loop, so it must not block on an
//! `mpsc` round-trip. The `mpsc` + `oneshot` path is reserved for the write
//! ([`SimplexRequest::Propose`]) and for explicit point-in-time queries
//! ([`SimplexRequest::IsLeader`]).

use async_trait::async_trait;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_protocol::L2BlockInfo;
use tokio::sync::{mpsc, oneshot, watch};

use super::SimplexError;

/// A point-in-time snapshot of consensus state, published on the read-side
/// [`watch`] channel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsensusStatus {
    /// Whether this node is the leader of the current view.
    pub is_leader: bool,
    /// The latest finalized unsafe head agreed by consensus, if any.
    pub finalized_head: Option<L2BlockInfo>,
    /// The current consensus view number.
    pub view: u64,
}

/// Write side of the consensus surface: hand a freshly sealed payload envelope to
/// consensus to be proposed.
///
/// This is the redirect target for the sequencer seal pipeline's op-conductor
/// commit when the simplex path is authoritative. The acknowledgement means only
/// that consensus accepted the envelope for proposal — **not** that it was
/// notarized or finalized; those outcomes are observed on the read side.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ConsensusProposer: Send + Sync + std::fmt::Debug {
    /// Submits a sealed payload envelope to consensus to be proposed.
    async fn propose(&self, envelope: BaseExecutionPayloadEnvelope) -> Result<(), SimplexError>;
}

/// Read side of the consensus surface: observe leadership and the finalized head.
///
/// [`subscribe`](ConsensusStatusReader::subscribe) hands out a [`watch::Receiver`]
/// for lock-free, non-blocking sampling on the hot path;
/// [`is_leader`](ConsensusStatusReader::is_leader) is an explicit point-in-time
/// query for bootstrap/admin parity with `Conductor::leader`.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ConsensusStatusReader: Send + Sync + std::fmt::Debug {
    /// Subscribes to the consensus status snapshot stream.
    fn subscribe(&self) -> watch::Receiver<ConsensusStatus>;

    /// Returns whether this node is the leader of the current view.
    async fn is_leader(&self) -> Result<bool, SimplexError>;
}

/// Request sent to the [`SimplexActor`](super::SimplexActor).
#[derive(Debug)]
pub enum SimplexRequest {
    /// Leader-only: hand a freshly sealed envelope to consensus to be proposed.
    ///
    /// The acknowledgement signals only that consensus accepted the envelope for
    /// proposal, not that it was notarized or finalized.
    Propose {
        /// The sealed payload envelope to propose.
        envelope: Box<BaseExecutionPayloadEnvelope>,
        /// Response channel for the proposal acknowledgement.
        response_tx: oneshot::Sender<Result<(), SimplexError>>,
    },
    /// Point-in-time leadership check (bootstrap / admin parity with
    /// `Conductor::leader`). Steady-state readers should prefer the
    /// [`watch`] channel from [`ConsensusStatusReader::subscribe`].
    IsLeader {
        /// Response channel for the leadership result.
        response_tx: oneshot::Sender<Result<bool, SimplexError>>,
    },
}

/// Client used to communicate with the [`SimplexActor`](super::SimplexActor).
///
/// Holds the request sender and a receiver for the consensus status snapshot so
/// both the write path (`mpsc` + `oneshot`) and the read path (`watch`) are
/// reachable from one handle.
#[derive(Debug, Clone)]
pub struct SimplexClient {
    request_tx: mpsc::Sender<SimplexRequest>,
    status_rx: watch::Receiver<ConsensusStatus>,
}

impl SimplexClient {
    /// Creates a new simplex client.
    pub const fn new(
        request_tx: mpsc::Sender<SimplexRequest>,
        status_rx: watch::Receiver<ConsensusStatus>,
    ) -> Self {
        Self { request_tx, status_rx }
    }

    async fn send(&self, request: SimplexRequest) -> Result<(), SimplexError> {
        self.request_tx.send(request).await.map_err(|_| SimplexError::ChannelClosed)
    }
}

#[async_trait]
impl ConsensusProposer for SimplexClient {
    async fn propose(&self, envelope: BaseExecutionPayloadEnvelope) -> Result<(), SimplexError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.send(SimplexRequest::Propose { envelope: Box::new(envelope), response_tx }).await?;
        response_rx.await.map_err(|_| SimplexError::ResponseDropped)?
    }
}

#[async_trait]
impl ConsensusStatusReader for SimplexClient {
    fn subscribe(&self) -> watch::Receiver<ConsensusStatus> {
        self.status_rx.clone()
    }

    async fn is_leader(&self) -> Result<bool, SimplexError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.send(SimplexRequest::IsLeader { response_tx }).await?;
        response_rx.await.map_err(|_| SimplexError::ResponseDropped)?
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use base_common_rpc_types_engine::BaseExecutionPayload;

    use super::*;

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

    #[tokio::test]
    async fn propose_maps_closed_channel_to_error() {
        let (request_tx, request_rx) = mpsc::channel(1);
        let (_status_tx, status_rx) = watch::channel(ConsensusStatus::default());
        let client = SimplexClient::new(request_tx, status_rx);
        drop(request_rx);

        let err = client.propose(test_envelope()).await.unwrap_err();
        assert!(matches!(err, SimplexError::ChannelClosed));
    }

    #[tokio::test]
    async fn is_leader_maps_closed_channel_to_error() {
        let (request_tx, request_rx) = mpsc::channel(1);
        let (_status_tx, status_rx) = watch::channel(ConsensusStatus::default());
        let client = SimplexClient::new(request_tx, status_rx);
        drop(request_rx);

        let err = client.is_leader().await.unwrap_err();
        assert!(matches!(err, SimplexError::ChannelClosed));
    }

    #[tokio::test]
    async fn subscribe_observes_published_status() {
        let (request_tx, _request_rx) = mpsc::channel(1);
        let (status_tx, status_rx) = watch::channel(ConsensusStatus::default());
        let client = SimplexClient::new(request_tx, status_rx);

        let mut rx = client.subscribe();
        status_tx.send_replace(ConsensusStatus { is_leader: true, finalized_head: None, view: 7 });
        assert!(rx.changed().await.is_ok());
        assert!(rx.borrow().is_leader);
        assert_eq!(rx.borrow().view, 7);
    }
}
