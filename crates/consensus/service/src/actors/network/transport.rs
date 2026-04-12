//! Gossip transport abstraction for the network actor.

use alloy_primitives::Address;
use async_trait::async_trait;
use base_alloy_rpc_types_engine::{BaseExecutionPayloadEnvelope, NetworkPayloadEnvelope};
use base_consensus_gossip::P2pRpcRequest;

/// Abstracts the gossip and discovery networking backend used by the [`crate::NetworkActor`].
///
/// The production implementation is [`crate::NetworkHandler`], which binds a real libp2p TCP
/// listener and a discv5 UDP socket. Test implementations can use in-process channels to deliver
/// blocks between actors without opening real network ports.
#[async_trait]
pub trait GossipTransport: Send + 'static {
    /// The error type for transport operations.
    type Error: std::fmt::Debug + Send + 'static;

    /// Publishes an [`BaseExecutionPayloadEnvelope`] to the gossip network.
    ///
    /// Implementations are responsible for signing the payload before publishing it.
    async fn publish(&mut self, payload: BaseExecutionPayloadEnvelope) -> Result<(), Self::Error>;

    /// Drives the transport event loop and returns the next unsafe block received from peers.
    ///
    /// Discovery (ENR reception, dialing) and peer monitoring are handled internally by the
    /// implementation. Returns `None` when the underlying transport has closed.
    async fn next_unsafe_block(&mut self) -> Option<NetworkPayloadEnvelope>;

    /// Updates the unsafe block signer address used to validate inbound gossip.
    fn set_block_signer(&mut self, address: Address);

    /// Dispatches a P2P RPC request to the underlying transport.
    ///
    /// Implementations that do not support P2P RPC (e.g. test transports) may ignore this.
    fn handle_p2p_rpc(&mut self, request: P2pRpcRequest);
}
