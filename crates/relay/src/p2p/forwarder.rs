//! Transaction forwarder for non-sequencer relay nodes.
//!
//! The forwarder receives validated encrypted transaction requests from the RPC layer
//! and forwards them to sequencer peers via the P2P network.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use alloy_primitives::B256;
use tokio::sync::{mpsc, oneshot};

use super::discovery::{ConnectionPeerId, SequencerPeerTracker};
use super::messages::EncryptedTxMessage;
use crate::pow;
use crate::types::EncryptedTransactionRequest;

/// Request to forward an encrypted transaction via P2P.
#[derive(Debug)]
pub struct ForwardRequest {
    /// The encrypted transaction request from RPC.
    pub request: EncryptedTransactionRequest,
    /// Channel to send the result back to the RPC handler.
    pub response_tx: oneshot::Sender<ForwardResult>,
}

/// Result of a forwarding attempt.
#[derive(Debug, Clone)]
pub enum ForwardResult {
    /// Successfully sent to at least one sequencer.
    Sent {
        /// Commitment hash for tracking.
        commitment: B256,
    },
    /// No sequencer peers are currently available.
    NoSequencers,
    /// Network error occurred.
    NetworkError(String),
}

/// Callback trait for sending messages to peers.
///
/// This is implemented by the network layer to actually send P2P messages.
pub trait PeerSender: Send + Sync + 'static {
    /// Sends an encrypted transaction message to a peer.
    ///
    /// The `connection_peer_id` is the 64-byte RLPx peer ID.
    /// Returns `true` if the message was queued successfully.
    fn send_encrypted_tx(&self, connection_peer_id: ConnectionPeerId, msg: EncryptedTxMessage) -> bool;
}

/// Background task that forwards encrypted transactions to sequencers.
pub struct RelayForwarder<S> {
    /// Receiver for forward requests from RPC.
    request_rx: mpsc::Receiver<ForwardRequest>,
    /// Tracks sequencer peers discovered via ENR attestations.
    sequencer_tracker: Arc<SequencerPeerTracker>,
    /// Network sender for P2P messages.
    sender: Arc<S>,
    /// Counter for generating unique request IDs.
    next_request_id: AtomicU64,
}

impl<S: PeerSender> RelayForwarder<S> {
    /// Creates a new forwarder.
    pub fn new(
        request_rx: mpsc::Receiver<ForwardRequest>,
        sequencer_tracker: Arc<SequencerPeerTracker>,
        sender: Arc<S>,
    ) -> Self {
        Self {
            request_rx,
            sequencer_tracker,
            sender,
            next_request_id: AtomicU64::new(1),
        }
    }

    /// Runs the forwarder task until the channel is closed.
    pub async fn run(mut self) {
        tracing::info!("Relay forwarder started");

        while let Some(req) = self.request_rx.recv().await {
            let result = self.forward(&req.request);
            let _ = req.response_tx.send(result);
        }

        tracing::info!("Relay forwarder stopped");
    }

    /// Forwards a request to sequencer peers.
    fn forward(&self, request: &EncryptedTransactionRequest) -> ForwardResult {
        let sequencers = self.sequencer_tracker.sequencer_connection_peers();

        if sequencers.is_empty() {
            tracing::warn!("No sequencer peers available for forwarding");
            return ForwardResult::NoSequencers;
        }

        let commitment = B256::from(pow::commitment(&request.encrypted_payload));
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);

        let msg = EncryptedTxMessage {
            request_id,
            encrypted_payload: request.encrypted_payload.clone(),
            pow_nonce: request.pow_nonce,
            encryption_pubkey: request.encryption_pubkey.clone(),
        };

        // Try to send to at least one sequencer
        let mut sent = false;
        for conn_peer_id in sequencers {
            if self.sender.send_encrypted_tx(conn_peer_id, msg.clone()) {
                tracing::debug!(
                    request_id = request_id,
                    peer = %alloy_primitives::hex::encode(&conn_peer_id[..8]),
                    commitment = %commitment,
                    "Forwarded encrypted transaction to sequencer"
                );
                sent = true;
                // For now, send to first available. Could broadcast to all.
                break;
            }
        }

        if sent {
            ForwardResult::Sent { commitment }
        } else {
            ForwardResult::NetworkError("failed to send to any sequencer".into())
        }
    }
}

/// Creates a channel pair for forwarding requests.
///
/// Returns `(sender, receiver)` where:
/// - `sender` is given to the RPC layer to submit requests
/// - `receiver` is given to the `RelayForwarder`
pub fn create_forward_channel(buffer: usize) -> (mpsc::Sender<ForwardRequest>, mpsc::Receiver<ForwardRequest>) {
    mpsc::channel(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use std::sync::atomic::AtomicBool;

    struct MockSender {
        should_succeed: AtomicBool,
        sent_count: AtomicU64,
    }

    impl MockSender {
        fn new(should_succeed: bool) -> Self {
            Self {
                should_succeed: AtomicBool::new(should_succeed),
                sent_count: AtomicU64::new(0),
            }
        }
    }

    impl PeerSender for MockSender {
        fn send_encrypted_tx(&self, _connection_peer_id: ConnectionPeerId, _msg: EncryptedTxMessage) -> bool {
            self.sent_count.fetch_add(1, Ordering::Relaxed);
            self.should_succeed.load(Ordering::Relaxed)
        }
    }

    fn create_test_tracker() -> Arc<SequencerPeerTracker> {
        use crate::keys::SequencerKeypair;
        use crate::attestation::create_attestation;

        let keypair = SequencerKeypair::generate();
        let params = crate::types::RelayParameters {
            encryption_pubkey: Bytes::copy_from_slice(&keypair.x25519_public_key()),
            previous_encryption_pubkey: None,
            pow_difficulty: 18,
            attestation_pubkey: Bytes::copy_from_slice(&keypair.ed25519_public_key()),
            attestation_validity_seconds: 86400,
        };
        let config = Arc::new(crate::RelayConfigCache::new(params));
        let tracker = Arc::new(SequencerPeerTracker::new(config));

        // Add a test sequencer
        let peer_id = B256::repeat_byte(0x55);
        let attestation = create_attestation(peer_id, &keypair);
        tracker.validate_and_add_peer(peer_id, &attestation).unwrap();

        tracker
    }

    #[test]
    fn test_forward_success() {
        let tracker = create_test_tracker();
        let sender = Arc::new(MockSender::new(true));
        let (_tx, rx) = create_forward_channel(10);

        let forwarder = RelayForwarder::new(rx, tracker, sender.clone());

        let request = EncryptedTransactionRequest {
            encrypted_payload: Bytes::from_static(&[1, 2, 3, 4]),
            pow_nonce: 12345,
            encryption_pubkey: Bytes::from_static(&[0xab; 32]),
        };

        let result = forwarder.forward(&request);

        assert!(matches!(result, ForwardResult::Sent { .. }));
        assert_eq!(sender.sent_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_forward_no_sequencers() {
        use crate::keys::SequencerKeypair;

        let keypair = SequencerKeypair::generate();
        let params = crate::types::RelayParameters {
            encryption_pubkey: Bytes::copy_from_slice(&keypair.x25519_public_key()),
            previous_encryption_pubkey: None,
            pow_difficulty: 18,
            attestation_pubkey: Bytes::copy_from_slice(&keypair.ed25519_public_key()),
            attestation_validity_seconds: 86400,
        };
        let config = Arc::new(crate::RelayConfigCache::new(params));
        let tracker = Arc::new(SequencerPeerTracker::new(config));
        // Don't add any sequencers

        let sender = Arc::new(MockSender::new(true));
        let (_tx, rx) = create_forward_channel(10);

        let forwarder = RelayForwarder::new(rx, tracker, sender.clone());

        let request = EncryptedTransactionRequest {
            encrypted_payload: Bytes::from_static(&[1, 2, 3, 4]),
            pow_nonce: 12345,
            encryption_pubkey: Bytes::from_static(&[0xab; 32]),
        };

        let result = forwarder.forward(&request);

        assert!(matches!(result, ForwardResult::NoSequencers));
        assert_eq!(sender.sent_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_forward_network_error() {
        let tracker = create_test_tracker();
        let sender = Arc::new(MockSender::new(false)); // Sending fails
        let (_tx, rx) = create_forward_channel(10);

        let forwarder = RelayForwarder::new(rx, tracker, sender.clone());

        let request = EncryptedTransactionRequest {
            encrypted_payload: Bytes::from_static(&[1, 2, 3, 4]),
            pow_nonce: 12345,
            encryption_pubkey: Bytes::from_static(&[0xab; 32]),
        };

        let result = forwarder.forward(&request);

        assert!(matches!(result, ForwardResult::NetworkError(_)));
    }
}
