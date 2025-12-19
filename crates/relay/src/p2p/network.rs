//! Network implementation of peer sender for actual P2P operations.
//!
//! This module provides the bridge between the abstract `PeerSender` trait
//! and the actual network layer.

#![cfg(feature = "p2p")]

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use reth_network_api::PeerId;
use tokio::sync::mpsc;

use crate::attestation::create_attestation;
use crate::keys::SequencerKeypair;
use crate::types::RelayAttestation;
use super::discovery::SequencerPeerTracker;
use super::forwarder::PeerSender;
use super::handler::{ConnectionMessage, ProtocolEvent};
use super::messages::{EncryptedTxMessage, IdentityMessage};

/// Network implementation of `PeerSender` that sends messages over P2P.
///
/// This struct maintains a map of active peer connections and forwards
/// messages to them via their respective command channels.
#[derive(Debug)]
pub struct NetworkPeerSender {
    /// Map of peer ID to their command channel.
    connections: RwLock<HashMap<PeerId, mpsc::UnboundedSender<ConnectionMessage>>>,
}

impl NetworkPeerSender {
    /// Creates a new network peer sender.
    pub fn new() -> Self {
        Self {
            connections: RwLock::new(HashMap::new()),
        }
    }

    /// Registers a new peer connection.
    pub fn add_peer(&self, peer_id: PeerId, sender: mpsc::UnboundedSender<ConnectionMessage>) {
        tracing::debug!(peer = %peer_id, "Added peer connection");
        self.connections.write().insert(peer_id, sender);
    }

    /// Removes a peer connection.
    pub fn remove_peer(&self, peer_id: &PeerId) {
        tracing::debug!(peer = %peer_id, "Removed peer connection");
        self.connections.write().remove(peer_id);
    }

    /// Checks if a peer is connected.
    pub fn is_connected(&self, peer_id: &PeerId) -> bool {
        self.connections.read().contains_key(peer_id)
    }

    /// Returns the number of connected peers.
    pub fn peer_count(&self) -> usize {
        self.connections.read().len()
    }
}

impl Default for NetworkPeerSender {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerSender for NetworkPeerSender {
    fn send_encrypted_tx(
        &self,
        connection_peer_id: super::discovery::ConnectionPeerId,
        msg: EncryptedTxMessage,
    ) -> bool {
        let connections = self.connections.read();

        // Find connection by matching the 64-byte connection peer ID
        for (stored_peer_id, sender) in connections.iter() {
            if stored_peer_id.as_slice() == connection_peer_id.as_slice() {
                match sender.send(ConnectionMessage::SendEncryptedTx(msg)) {
                    Ok(()) => return true,
                    Err(e) => {
                        tracing::warn!(
                            peer = %alloy_primitives::hex::encode(&connection_peer_id[..8]),
                            error = %e,
                            "Failed to send encrypted tx to peer"
                        );
                        return false;
                    }
                }
            }
        }

        tracing::warn!(
            peer = %alloy_primitives::hex::encode(&connection_peer_id[..8]),
            "Peer not found in connections"
        );
        false
    }
}

/// Background task that handles protocol events and manages peer connections.
///
/// This task receives protocol events from the handler and updates the
/// NetworkPeerSender and SequencerPeerTracker accordingly.
pub struct ProtocolEventHandler {
    /// Receiver for protocol events.
    events_rx: mpsc::UnboundedReceiver<ProtocolEvent>,
    /// Network sender for peer management.
    network_sender: Arc<NetworkPeerSender>,
    /// Tracker for sequencer peers.
    sequencer_tracker: Arc<SequencerPeerTracker>,
}

impl ProtocolEventHandler {
    /// Creates a new protocol event handler.
    pub fn new(
        events_rx: mpsc::UnboundedReceiver<ProtocolEvent>,
        network_sender: Arc<NetworkPeerSender>,
        sequencer_tracker: Arc<SequencerPeerTracker>,
    ) -> Self {
        Self {
            events_rx,
            network_sender,
            sequencer_tracker,
        }
    }

    /// Runs the event handler until the channel is closed.
    ///
    /// For relay mode, this processes Established/Disconnected events to manage
    /// peer connections and tracks sequencer peers via ENR attestations.
    pub async fn run(mut self) {
        tracing::info!("Protocol event handler started");

        while let Some(event) = self.events_rx.recv().await {
            match event {
                ProtocolEvent::Established {
                    peer_id,
                    to_connection,
                } => {
                    tracing::info!(peer = %peer_id, "Peer established encrypted-relay connection");
                    self.network_sender.add_peer(peer_id, to_connection);

                    // TODO: Check ENR for relay-attest key and validate attestation
                    // For now, we need to manually add sequencers via other means
                }
                ProtocolEvent::Disconnected { peer_id } => {
                    tracing::info!(peer = %peer_id, "Peer disconnected from encrypted-relay");
                    self.network_sender.remove_peer(&peer_id);

                    // Also remove from sequencer tracker if tracked (by connection peer ID)
                    self.sequencer_tracker.remove_peer_by_connection(peer_id.as_slice());
                }
                ProtocolEvent::ReceivedEncryptedTx(received) => {
                    // This is handled by SequencerProcessor in sequencer mode
                    tracing::debug!(
                        peer = %received.peer_id,
                        request_id = received.message.request_id,
                        "Received encrypted tx (relay mode should not see this)"
                    );
                }
                ProtocolEvent::ReceivedAck { peer_id, ack } => {
                    tracing::debug!(
                        peer = %peer_id,
                        request_id = ack.request_id,
                        accepted = ack.accepted,
                        "Received ack from sequencer"
                    );
                    // TODO: Implement ack handling (e.g., for request tracking)
                }
                ProtocolEvent::ReceivedIdentity { peer_id, identity } => {
                    tracing::info!(
                        peer = %peer_id,
                        node_id = %identity.node_id,
                        timestamp = identity.timestamp,
                        "Received identity from peer"
                    );

                    // Convert the IdentityMessage to a RelayAttestation for validation
                    let attestation = RelayAttestation {
                        node_id: identity.node_id,
                        timestamp: identity.timestamp,
                        signature: identity.signature,
                    };

                    // Use the new method that tracks both node_id and connection peer_id
                    match self.sequencer_tracker.validate_and_add_peer_with_connection(
                        peer_id.as_slice(),
                        &attestation,
                    ) {
                        Ok(true) => {
                            tracing::info!(
                                peer = %peer_id,
                                node_id = %identity.node_id,
                                "Validated and added sequencer peer"
                            );
                        }
                        Ok(false) => {
                            tracing::debug!(
                                peer = %peer_id,
                                node_id = %identity.node_id,
                                "Peer identity not added (already tracked or not sequencer)"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                peer = %peer_id,
                                node_id = %identity.node_id,
                                error = %e,
                                "Failed to validate peer identity attestation"
                            );
                        }
                    }
                }
            }
        }

        tracing::info!("Protocol event handler stopped");
    }
}

/// Background task that handles protocol events for sequencer mode.
///
/// Similar to ProtocolEventHandler but specifically routes received encrypted
/// transactions to the sequencer processor.
pub struct SequencerEventHandler {
    /// Receiver for protocol events.
    events_rx: mpsc::UnboundedReceiver<ProtocolEvent>,
    /// Channel to send received transactions to the sequencer processor.
    incoming_tx: mpsc::Sender<super::sequencer::ReceivedEncryptedTx>,
    /// Network sender for peer management.
    network_sender: Arc<NetworkPeerSender>,
    /// Sequencer keypair for creating attestations (identifies this node as sequencer).
    keypair: Option<Arc<SequencerKeypair>>,
}

impl SequencerEventHandler {
    /// Creates a new sequencer event handler.
    pub fn new(
        events_rx: mpsc::UnboundedReceiver<ProtocolEvent>,
        incoming_tx: mpsc::Sender<super::sequencer::ReceivedEncryptedTx>,
        network_sender: Arc<NetworkPeerSender>,
    ) -> Self {
        Self {
            events_rx,
            incoming_tx,
            network_sender,
            keypair: None,
        }
    }

    /// Creates a new sequencer event handler with a keypair for identity announcements.
    pub fn with_keypair(
        events_rx: mpsc::UnboundedReceiver<ProtocolEvent>,
        incoming_tx: mpsc::Sender<super::sequencer::ReceivedEncryptedTx>,
        network_sender: Arc<NetworkPeerSender>,
        keypair: Arc<SequencerKeypair>,
    ) -> Self {
        Self {
            events_rx,
            incoming_tx,
            network_sender,
            keypair: Some(keypair),
        }
    }

    /// Runs the event handler until the channel is closed.
    pub async fn run(mut self) {
        tracing::info!("Sequencer event handler started");

        while let Some(event) = self.events_rx.recv().await {
            match event {
                ProtocolEvent::Established {
                    peer_id,
                    to_connection,
                } => {
                    tracing::info!(peer = %peer_id, "Relay peer connected to sequencer");
                    self.network_sender.add_peer(peer_id, to_connection.clone());

                    // Send identity announcement if we have a keypair
                    if let Some(ref keypair) = self.keypair {
                        // Use our Ed25519 public key as node_id (32 bytes)
                        let node_id = alloy_primitives::B256::from(keypair.ed25519_public_key());
                        let attestation = create_attestation(node_id, keypair);

                        let identity_msg = IdentityMessage {
                            node_id: attestation.node_id,
                            timestamp: attestation.timestamp,
                            signature: attestation.signature,
                        };

                        tracing::info!(
                            peer = %peer_id,
                            node_id = %identity_msg.node_id,
                            "Sending identity announcement to peer"
                        );

                        if let Err(e) = to_connection.send(ConnectionMessage::SendIdentity(identity_msg)) {
                            tracing::warn!(
                                peer = %peer_id,
                                error = %e,
                                "Failed to send identity announcement"
                            );
                        }
                    }
                }
                ProtocolEvent::Disconnected { peer_id } => {
                    tracing::info!(peer = %peer_id, "Relay peer disconnected from sequencer");
                    self.network_sender.remove_peer(&peer_id);
                }
                ProtocolEvent::ReceivedEncryptedTx(received) => {
                    tracing::debug!(
                        peer = %received.peer_id,
                        request_id = received.message.request_id,
                        "Received encrypted tx from relay"
                    );
                    if let Err(e) = self.incoming_tx.send(received).await {
                        tracing::warn!(error = %e, "Failed to forward encrypted tx to processor");
                    }
                }
                ProtocolEvent::ReceivedAck { peer_id, ack } => {
                    // Sequencer shouldn't receive acks, log for debugging
                    tracing::warn!(
                        peer = %peer_id,
                        request_id = ack.request_id,
                        "Sequencer received unexpected ack"
                    );
                }
                ProtocolEvent::ReceivedIdentity { peer_id, identity } => {
                    // Sequencer receives identity from other nodes (which may be relays)
                    // We don't need to validate relay identities, just log
                    tracing::debug!(
                        peer = %peer_id,
                        node_id = %identity.node_id,
                        "Received identity from relay peer (ignored in sequencer mode)"
                    );
                }
            }
        }

        tracing::info!("Sequencer event handler stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_peer_sender_add_remove() {
        let sender = NetworkPeerSender::new();
        let (tx, _rx) = mpsc::unbounded_channel();

        // Use a fixed peer ID for testing
        let peer_bytes = [0x11u8; 64];
        let peer_id = PeerId::from_slice(&peer_bytes);

        assert_eq!(sender.peer_count(), 0);
        assert!(!sender.is_connected(&peer_id));

        sender.add_peer(peer_id, tx);
        assert_eq!(sender.peer_count(), 1);
        assert!(sender.is_connected(&peer_id));

        sender.remove_peer(&peer_id);
        assert_eq!(sender.peer_count(), 0);
        assert!(!sender.is_connected(&peer_id));
    }
}
