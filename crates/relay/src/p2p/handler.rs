//! RLPx subprotocol handler for encrypted transaction relay.
//!
//! This module implements the custom devp2p subprotocol for forwarding encrypted
//! transactions between relay nodes and the sequencer.

#![cfg(feature = "p2p")]

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use alloy_rlp::{Decodable, Encodable};
use bytes::{Buf, BufMut, BytesMut};
use futures::{Stream, StreamExt};
use pin_project::pin_project;
use reth_eth_wire::{
    capability::SharedCapabilities,
    multiplex::ProtocolConnection,
    protocol::Protocol,
    Capability,
};
use reth_network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_network_api::{Direction, PeerId};
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use super::messages::{AckMessage, EncryptedTxMessage, IdentityMessage, RelayMessageId, PROTOCOL_NAME, PROTOCOL_VERSION};
use super::sequencer::ReceivedEncryptedTx;

/// Number of messages in our protocol (EncryptedTx = 0x00, Ack = 0x01, Identity = 0x02).
const PROTOCOL_MESSAGE_COUNT: u8 = 3;

/// Commands that can be sent to a connection.
#[derive(Debug, Clone)]
pub enum ConnectionMessage {
    /// Send an encrypted transaction to this peer (relay -> sequencer).
    SendEncryptedTx(EncryptedTxMessage),
    /// Send an acknowledgment to this peer (sequencer -> relay).
    SendAck(AckMessage),
    /// Send an identity announcement to this peer (sequencer -> relay).
    SendIdentity(IdentityMessage),
}

/// Events emitted by the protocol handler.
#[derive(Debug)]
pub enum ProtocolEvent {
    /// A new peer connection was established that supports our protocol.
    Established {
        /// The peer's ID.
        peer_id: PeerId,
        /// Channel to send messages to this peer.
        to_connection: mpsc::UnboundedSender<ConnectionMessage>,
    },
    /// A peer disconnected.
    Disconnected {
        /// The peer's ID.
        peer_id: PeerId,
    },
    /// Received an encrypted transaction from a peer (sequencer mode).
    ReceivedEncryptedTx(ReceivedEncryptedTx),
    /// Received an acknowledgment from a peer (relay mode).
    ReceivedAck {
        /// The peer that sent the ack.
        peer_id: PeerId,
        /// The acknowledgment message.
        ack: AckMessage,
    },
    /// Received an identity announcement from a peer (relay mode).
    ReceivedIdentity {
        /// The peer that sent the identity.
        peer_id: PeerId,
        /// The identity message containing attestation.
        identity: IdentityMessage,
    },
}

/// Shared state for the protocol.
pub struct ProtocolState {
    /// Channel to emit protocol events.
    pub events_tx: mpsc::UnboundedSender<ProtocolEvent>,
}

impl std::fmt::Debug for ProtocolState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProtocolState").finish()
    }
}

/// The encrypted relay protocol handler.
///
/// This is registered with reth's network layer and handles incoming/outgoing
/// connections for the encrypted-relay subprotocol.
#[derive(Debug)]
pub struct EncryptedRelayProtoHandler {
    /// Shared protocol state.
    state: Arc<ProtocolState>,
}

impl EncryptedRelayProtoHandler {
    /// Creates a new protocol handler.
    pub fn new(events_tx: mpsc::UnboundedSender<ProtocolEvent>) -> Self {
        Self {
            state: Arc::new(ProtocolState { events_tx }),
        }
    }

    /// Returns the protocol capability.
    pub fn capability() -> Capability {
        Capability::new_static(PROTOCOL_NAME, PROTOCOL_VERSION as usize)
    }

    /// Returns the protocol definition.
    pub fn protocol() -> Protocol {
        Protocol::new(Self::capability(), PROTOCOL_MESSAGE_COUNT)
    }
}

impl ProtocolHandler for EncryptedRelayProtoHandler {
    type ConnectionHandler = EncryptedRelayConnectionHandler;

    fn on_incoming(&self, _socket_addr: SocketAddr) -> Option<Self::ConnectionHandler> {
        Some(EncryptedRelayConnectionHandler::new(self.state.clone()))
    }

    fn on_outgoing(
        &self,
        _socket_addr: SocketAddr,
        _peer_id: PeerId,
    ) -> Option<Self::ConnectionHandler> {
        Some(EncryptedRelayConnectionHandler::new(self.state.clone()))
    }
}

/// Handler for an individual connection.
pub struct EncryptedRelayConnectionHandler {
    /// Shared protocol state.
    state: Arc<ProtocolState>,
    /// Sender for commands (given to ProtocolEvent::Established).
    commands_tx: mpsc::UnboundedSender<ConnectionMessage>,
    /// Receiver for commands to send to this peer.
    commands_rx: mpsc::UnboundedReceiver<ConnectionMessage>,
}

impl std::fmt::Debug for EncryptedRelayConnectionHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedRelayConnectionHandler").finish()
    }
}

impl EncryptedRelayConnectionHandler {
    /// Creates a new connection handler.
    fn new(state: Arc<ProtocolState>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            state,
            commands_tx: tx,
            commands_rx: rx,
        }
    }
}

impl ConnectionHandler for EncryptedRelayConnectionHandler {
    type Connection = EncryptedRelayConnection;

    fn protocol(&self) -> Protocol {
        EncryptedRelayProtoHandler::protocol()
    }

    fn on_unsupported_by_peer(
        self,
        _supported: &SharedCapabilities,
        _direction: Direction,
        _peer_id: PeerId,
    ) -> OnNotSupported {
        // If peer doesn't support our protocol, just keep the connection alive
        // (they may support eth protocol which is the primary)
        OnNotSupported::KeepAlive
    }

    fn into_connection(
        self,
        direction: Direction,
        peer_id: PeerId,
        conn: ProtocolConnection,
    ) -> Self::Connection {
        tracing::info!(
            peer = %peer_id,
            direction = ?direction,
            "Creating EncryptedRelayConnection"
        );

        // Emit the Established event with the command channel
        let _ = self.state.events_tx.send(ProtocolEvent::Established {
            peer_id,
            to_connection: self.commands_tx,
        });

        EncryptedRelayConnection::new(
            peer_id,
            conn,
            self.commands_rx,
            self.state.events_tx.clone(),
        )
    }
}

/// Active connection for the encrypted relay protocol.
///
/// This is a stream that:
/// - Receives incoming messages from `ProtocolConnection`
/// - Yields outgoing messages to send on the wire
/// - Handles command channel for sending messages
#[pin_project]
pub struct EncryptedRelayConnection {
    /// The peer we're connected to.
    peer_id: PeerId,
    /// Incoming messages from the wire.
    #[pin]
    incoming: ProtocolConnection,
    /// Commands to send outgoing messages.
    #[pin]
    commands: UnboundedReceiverStream<ConnectionMessage>,
    /// Channel to emit events (for incoming message handling).
    events_tx: mpsc::UnboundedSender<ProtocolEvent>,
    /// Whether we've notified about disconnect.
    disconnected: bool,
}

impl EncryptedRelayConnection {
    /// Creates a new connection.
    fn new(
        peer_id: PeerId,
        incoming: ProtocolConnection,
        commands_rx: mpsc::UnboundedReceiver<ConnectionMessage>,
        events_tx: mpsc::UnboundedSender<ProtocolEvent>,
    ) -> Self {
        Self {
            peer_id,
            incoming,
            commands: UnboundedReceiverStream::new(commands_rx),
            events_tx,
            disconnected: false,
        }
    }

    /// Encodes a message with its ID prefix for sending over the wire.
    fn encode_message<M: Encodable>(msg_id: u8, msg: &M) -> BytesMut {
        let mut buf = BytesMut::new();
        buf.put_u8(msg_id);
        msg.encode(&mut buf);
        buf
    }

    /// Decodes a message from the wire format.
    fn decode_message(data: &[u8]) -> Result<RelayMessage, String> {
        if data.is_empty() {
            return Err("empty message".into());
        }

        let msg_id = data[0];
        let payload = &data[1..];

        match RelayMessageId::try_from(msg_id) {
            Ok(RelayMessageId::EncryptedTx) => {
                let msg = EncryptedTxMessage::decode(&mut &payload[..])
                    .map_err(|e| format!("failed to decode EncryptedTxMessage: {e}"))?;
                Ok(RelayMessage::EncryptedTx(msg))
            }
            Ok(RelayMessageId::Ack) => {
                let msg = AckMessage::decode(&mut &payload[..])
                    .map_err(|e| format!("failed to decode AckMessage: {e}"))?;
                Ok(RelayMessage::Ack(msg))
            }
            Ok(RelayMessageId::Identity) => {
                let msg = IdentityMessage::decode(&mut &payload[..])
                    .map_err(|e| format!("failed to decode IdentityMessage: {e}"))?;
                Ok(RelayMessage::Identity(msg))
            }
            Err(_) => Err(format!("unknown message id: {msg_id}")),
        }
    }

    /// Handles an incoming message from the wire.
    #[allow(dead_code)]
    fn handle_incoming(&self, mut data: BytesMut) {
        match Self::decode_message(&data) {
            Ok(RelayMessage::EncryptedTx(msg)) => {
                // Hash the 64-byte PeerId to get a 32-byte identifier
                let peer_id_hash = alloy_primitives::keccak256(self.peer_id.as_slice());
                let received = ReceivedEncryptedTx {
                    peer_id: peer_id_hash,
                    message: msg,
                };
                let _ = self.events_tx.send(ProtocolEvent::ReceivedEncryptedTx(received));
            }
            Ok(RelayMessage::Ack(ack)) => {
                let _ = self.events_tx.send(ProtocolEvent::ReceivedAck {
                    peer_id: self.peer_id,
                    ack,
                });
            }
            Ok(RelayMessage::Identity(identity)) => {
                let _ = self.events_tx.send(ProtocolEvent::ReceivedIdentity {
                    peer_id: self.peer_id,
                    identity,
                });
            }
            Err(e) => {
                tracing::warn!(peer = %self.peer_id, error = %e, "Failed to decode relay message");
            }
        }

        // Consume all bytes
        data.advance(data.len());
    }
}

/// Decoded relay message.
enum RelayMessage {
    EncryptedTx(EncryptedTxMessage),
    Ack(AckMessage),
    Identity(IdentityMessage),
}

impl Stream for EncryptedRelayConnection {
    type Item = BytesMut;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        // First, check for outgoing commands
        match this.commands.as_mut().poll_next(cx) {
            Poll::Ready(None) => {
                // Commands channel closed - sender was dropped
                tracing::warn!(
                    peer = %*this.peer_id,
                    "Commands channel closed - connection may be orphaned"
                );
            }
            Poll::Pending => {}
            Poll::Ready(Some(cmd)) => {
                let msg_type = match &cmd {
                    ConnectionMessage::SendEncryptedTx(_) => "EncryptedTx",
                    ConnectionMessage::SendAck(_) => "Ack",
                    ConnectionMessage::SendIdentity(_) => "Identity",
                };
                tracing::debug!(
                    peer = %*this.peer_id,
                    message_type = msg_type,
                    "Sending outgoing message"
                );
                let encoded = match cmd {
                    ConnectionMessage::SendEncryptedTx(msg) => {
                        Self::encode_message(EncryptedTxMessage::message_id(), &msg)
                    }
                    ConnectionMessage::SendAck(msg) => {
                        Self::encode_message(AckMessage::message_id(), &msg)
                    }
                    ConnectionMessage::SendIdentity(msg) => {
                        Self::encode_message(IdentityMessage::message_id(), &msg)
                    }
                };
                return Poll::Ready(Some(encoded));
            }
        }

        // Then, poll for incoming messages and handle them
        match this.incoming.as_mut().poll_next(cx) {
            Poll::Ready(Some(data)) => {
                // Handle the incoming message (emits event)
                // Note: We use a helper method since we can't call methods on projections
                let peer_id = *this.peer_id;
                let events_tx = this.events_tx.clone();

                match EncryptedRelayConnection::decode_message(&data) {
                    Ok(RelayMessage::EncryptedTx(msg)) => {
                        // Hash the 64-byte PeerId to get a 32-byte identifier
                        let peer_id_hash = alloy_primitives::keccak256(peer_id.as_slice());
                        let received = ReceivedEncryptedTx {
                            peer_id: peer_id_hash,
                            message: msg,
                        };
                        let _ = events_tx.send(ProtocolEvent::ReceivedEncryptedTx(received));
                    }
                    Ok(RelayMessage::Ack(ack)) => {
                        let _ = events_tx.send(ProtocolEvent::ReceivedAck { peer_id, ack });
                    }
                    Ok(RelayMessage::Identity(identity)) => {
                        let _ = events_tx.send(ProtocolEvent::ReceivedIdentity { peer_id, identity });
                    }
                    Err(e) => {
                        tracing::warn!(peer = %peer_id, error = %e, "Failed to decode relay message");
                    }
                }

                // Continue polling (we handled the message, but don't yield anything)
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(None) => {
                // Connection closed, emit disconnect event
                if !*this.disconnected {
                    *this.disconnected = true;
                    tracing::warn!(
                        peer = %*this.peer_id,
                        "ProtocolConnection stream ended - connection closing"
                    );
                    let _ = this.events_tx.send(ProtocolEvent::Disconnected {
                        peer_id: *this.peer_id,
                    });
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_encrypted_tx() {
        let msg = EncryptedTxMessage {
            request_id: 42,
            encrypted_payload: alloy_primitives::Bytes::from_static(&[1, 2, 3]),
            pow_nonce: 12345,
            encryption_pubkey: alloy_primitives::Bytes::from_static(&[0xab; 32]),
        };

        let encoded = EncryptedRelayConnection::encode_message(EncryptedTxMessage::message_id(), &msg);
        let decoded = EncryptedRelayConnection::decode_message(&encoded).unwrap();

        match decoded {
            RelayMessage::EncryptedTx(decoded_msg) => {
                assert_eq!(msg, decoded_msg);
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_encode_decode_ack() {
        let msg = AckMessage::success(42, alloy_primitives::B256::repeat_byte(0xcd));

        let encoded = EncryptedRelayConnection::encode_message(AckMessage::message_id(), &msg);
        let decoded = EncryptedRelayConnection::decode_message(&encoded).unwrap();

        match decoded {
            RelayMessage::Ack(decoded_msg) => {
                assert_eq!(msg, decoded_msg);
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_capability() {
        let cap = EncryptedRelayProtoHandler::capability();
        assert_eq!(cap.name.as_ref(), PROTOCOL_NAME);
        assert_eq!(cap.version, PROTOCOL_VERSION as usize);
    }
}
