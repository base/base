//! In-process gossip transport for action tests.
//!
//! Provides a channel-backed [`GossipTransport`] implementation that routes
//! blocks between test actors without opening real network ports.

use alloy_primitives::{Address, B256, Signature, U256};
use async_trait::async_trait;
use base_alloy_rpc_types_engine::{
    OpExecutionPayloadEnvelope, OpNetworkPayloadEnvelope, PayloadHash,
};
use base_consensus_gossip::P2pRpcRequest;
use base_consensus_node::GossipTransport;
use tokio::sync::mpsc;

/// Handle for injecting blocks into a [`TestGossipTransport`].
///
/// Held by test code or the sequencer. Call [`send`] to deliver an
/// [`OpNetworkPayloadEnvelope`] to the matching [`TestGossipTransport`].
///
/// [`send`]: SupervisedP2P::send
#[derive(Debug, Clone)]
pub struct SupervisedP2P {
    tx: mpsc::UnboundedSender<OpNetworkPayloadEnvelope>,
}

impl SupervisedP2P {
    /// Send an [`OpNetworkPayloadEnvelope`] into the transport channel.
    pub fn send(&self, payload: OpNetworkPayloadEnvelope) {
        let _ = self.tx.send(payload);
    }
}

/// Channel-backed [`GossipTransport`] for action tests.
///
/// Routes blocks between test actors in-process without touching the network.
/// Use [`channel`] to construct the [`SupervisedP2P`] / [`TestGossipTransport`]
/// pair.
///
/// In a single-node test, [`publish`] routes directly to [`next_unsafe_block`]
/// via the internal channel. In a two-node test, the sequencer holds a
/// [`SupervisedP2P`] handle and this transport is held by the node under test.
///
/// [`channel`]: TestGossipTransport::channel
/// [`publish`]: GossipTransport::publish
/// [`next_unsafe_block`]: GossipTransport::next_unsafe_block
#[derive(Debug)]
pub struct TestGossipTransport {
    tx: mpsc::UnboundedSender<OpNetworkPayloadEnvelope>,
    rx: mpsc::UnboundedReceiver<OpNetworkPayloadEnvelope>,
}

impl TestGossipTransport {
    /// Create a [`SupervisedP2P`] / [`TestGossipTransport`] pair sharing a
    /// single channel.
    ///
    /// The [`SupervisedP2P`] handle allows test code or the sequencer to inject
    /// blocks. The [`TestGossipTransport`] delivers them via
    /// [`next_unsafe_block`].
    ///
    /// [`next_unsafe_block`]: GossipTransport::next_unsafe_block
    pub fn channel() -> (SupervisedP2P, Self) {
        let (tx, rx) = mpsc::unbounded_channel();
        (SupervisedP2P { tx: tx.clone() }, Self { tx, rx })
    }

    /// Try to receive the next unsafe block without blocking.
    ///
    /// Returns `None` immediately if no block is currently available. Use this
    /// to drain the channel in a non-blocking loop inside [`TestRollupNode::step`].
    ///
    /// [`TestRollupNode::step`]: crate::TestRollupNode::step
    pub fn try_next_unsafe_block(&mut self) -> Option<OpNetworkPayloadEnvelope> {
        self.rx.try_recv().ok()
    }
}

/// Infallible error type for [`TestGossipTransport`].
#[derive(Debug)]
pub enum TestGossipTransportError {}

#[async_trait]
impl GossipTransport for TestGossipTransport {
    type Error = TestGossipTransportError;

    async fn publish(&mut self, payload: OpExecutionPayloadEnvelope) -> Result<(), Self::Error> {
        let parent_beacon_block_root = payload.parent_beacon_block_root;
        let network = OpNetworkPayloadEnvelope {
            payload: payload.execution_payload,
            signature: Signature::new(U256::ZERO, U256::ZERO, false),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root,
        };
        let _ = self.tx.send(network);
        Ok(())
    }

    async fn next_unsafe_block(&mut self) -> Option<OpNetworkPayloadEnvelope> {
        self.rx.recv().await
    }

    fn set_block_signer(&mut self, _address: Address) {}

    fn handle_p2p_rpc(&mut self, _request: P2pRpcRequest) {}
}
