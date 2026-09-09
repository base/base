//! Network types

use base_consensus_network_service::P2pRpcRequest;

/// A type alias for the sender of a [`P2pRpcRequest`].
type P2pReqSender = tokio::sync::mpsc::Sender<P2pRpcRequest>;

/// `P2pRpc`
///
/// This is a server implementation of [`crate::rpc::BaseP2PApiServer`].
#[derive(Debug)]
pub struct P2pRpc {
    /// The channel to send [`P2pRpcRequest`]s.
    pub sender: P2pReqSender,
}

impl P2pRpc {
    /// Constructs a new [`P2pRpc`] given a sender channel.
    pub const fn new(sender: P2pReqSender) -> Self {
        Self { sender }
    }
}
