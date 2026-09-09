use alloy_primitives::{
    B512 as PeerId,
    bytes::{Bytes, BytesMut},
};

/// Raw egress values for an ECIES protocol
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EgressECIESValue {
    /// The AUTH message being sent out
    Auth,
    /// The ACK message being sent out
    Ack,
    /// The message being sent out (wrapped bytes)
    Message(Bytes),
}

/// Raw ingress values for an ECIES protocol
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngressECIESValue {
    /// Receiving a message from a [`PeerId`]
    AuthReceive(PeerId),
    /// Receiving an ACK message
    Ack,
    /// Receiving a message
    Message(BytesMut),
}
