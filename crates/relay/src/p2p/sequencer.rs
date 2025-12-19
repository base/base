//! Sequencer-side processing of encrypted transactions.
//!
//! The sequencer receives encrypted transactions via P2P, decrypts them,
//! validates them, and submits them to the transaction pool.

use std::sync::Arc;

use alloy_primitives::{B256, Bytes};
use tokio::sync::mpsc;

use super::discovery::PeerId;
use super::messages::{AckMessage, EncryptedTxMessage};
use crate::config::RelayConfigCache;
use crate::error::RelayError;
use crate::{crypto, pow};

/// An encrypted transaction received from a relay peer.
#[derive(Debug, Clone)]
pub struct ReceivedEncryptedTx {
    /// The peer that sent this transaction.
    pub peer_id: PeerId,
    /// The encrypted transaction message.
    pub message: EncryptedTxMessage,
}

/// Result of processing an encrypted transaction.
#[derive(Debug)]
pub struct ProcessResult {
    /// The peer to send acknowledgment to.
    pub peer_id: PeerId,
    /// The acknowledgment message.
    pub ack: AckMessage,
    /// The decrypted raw transaction bytes (if successful).
    pub raw_tx: Option<Bytes>,
}

/// Callback trait for submitting transactions to the pool and sending acks.
pub trait TransactionSubmitter: Send + Sync + 'static {
    /// Submits a raw transaction to the transaction pool.
    ///
    /// Returns `Ok(())` on success, or an error message on failure.
    fn submit_transaction(&self, raw_tx: Bytes) -> impl std::future::Future<Output = Result<(), String>> + Send;

    /// Sends an acknowledgment back to a peer.
    fn send_ack(&self, peer_id: PeerId, ack: AckMessage);
}

/// Processes encrypted transactions received from P2P peers.
pub struct SequencerProcessor<S> {
    /// Receiver for encrypted transactions from P2P layer.
    rx: mpsc::Receiver<ReceivedEncryptedTx>,
    /// X25519 secret key for decryption.
    secret_key: [u8; 32],
    /// Config cache for validation.
    config: Arc<RelayConfigCache>,
    /// Submitter for transactions and acks.
    submitter: Arc<S>,
}

impl<S: TransactionSubmitter> SequencerProcessor<S> {
    /// Creates a new sequencer processor.
    pub fn new(
        rx: mpsc::Receiver<ReceivedEncryptedTx>,
        secret_key: [u8; 32],
        config: Arc<RelayConfigCache>,
        submitter: Arc<S>,
    ) -> Self {
        Self {
            rx,
            secret_key,
            config,
            submitter,
        }
    }

    /// Runs the processor task until the channel is closed.
    pub async fn run(mut self) {
        tracing::info!("Sequencer processor started");

        while let Some(received) = self.rx.recv().await {
            self.process(received).await;
        }

        tracing::info!("Sequencer processor stopped");
    }

    /// Processes a single encrypted transaction.
    async fn process(&self, received: ReceivedEncryptedTx) {
        let commitment = B256::from(pow::commitment(&received.message.encrypted_payload));
        // Compute PoW hash for logging - shows leading zeros for verification
        let pow_hash = B256::from(pow::hash(
            &received.message.encrypted_payload,
            received.message.pow_nonce,
        ));

        match self.process_inner(&received.message).await {
            Ok(raw_tx) => {
                tracing::info!(
                    request_id = received.message.request_id,
                    commitment = %commitment,
                    pow_hash = %pow_hash,
                    tx_size = raw_tx.len(),
                    "Decrypted and submitted transaction"
                );

                // Send success ack
                let ack = AckMessage::success(received.message.request_id, commitment);
                self.submitter.send_ack(received.peer_id, ack);
            }
            Err(e) => {
                tracing::warn!(
                    request_id = received.message.request_id,
                    commitment = %commitment,
                    pow_hash = %pow_hash,
                    error = %e,
                    "Failed to process encrypted transaction"
                );

                // Send failure ack
                let ack = AckMessage::failure(
                    received.message.request_id,
                    commitment,
                    e.code() as u8,
                );
                self.submitter.send_ack(received.peer_id, ack);
            }
        }
    }

    /// Inner processing logic that returns Result for easier error handling.
    async fn process_inner(&self, msg: &EncryptedTxMessage) -> Result<Bytes, RelayError> {
        // Validate encryption key
        if !self.config.is_valid_encryption_key(&msg.encryption_pubkey) {
            return Err(RelayError::InvalidEncryptionKey);
        }

        // Validate PoW
        let params = self.config.get();
        pow::verify(&msg.encrypted_payload, msg.pow_nonce, params.pow_difficulty)?;

        // Decrypt the payload
        let raw_tx = crypto::decrypt(&self.secret_key, &msg.encrypted_payload)?;
        let raw_tx = Bytes::from(raw_tx);

        // Submit to transaction pool
        self.submitter
            .submit_transaction(raw_tx.clone())
            .await
            .map_err(|e| RelayError::Internal(format!("pool submission failed: {e}")))?;

        Ok(raw_tx)
    }
}

/// Creates a channel for receiving encrypted transactions.
pub fn create_receive_channel(buffer: usize) -> (mpsc::Sender<ReceivedEncryptedTx>, mpsc::Receiver<ReceivedEncryptedTx>) {
    mpsc::channel(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use parking_lot::Mutex;

    struct MockSubmitter {
        submit_count: AtomicU64,
        ack_count: AtomicU64,
        submitted_txs: Mutex<Vec<Bytes>>,
        should_fail: bool,
    }

    impl MockSubmitter {
        fn new(should_fail: bool) -> Self {
            Self {
                submit_count: AtomicU64::new(0),
                ack_count: AtomicU64::new(0),
                submitted_txs: Mutex::new(Vec::new()),
                should_fail,
            }
        }
    }

    impl TransactionSubmitter for MockSubmitter {
        async fn submit_transaction(&self, raw_tx: Bytes) -> Result<(), String> {
            self.submit_count.fetch_add(1, Ordering::Relaxed);
            if self.should_fail {
                Err("mock failure".into())
            } else {
                self.submitted_txs.lock().push(raw_tx);
                Ok(())
            }
        }

        fn send_ack(&self, _peer_id: PeerId, _ack: AckMessage) {
            self.ack_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn create_test_config_and_key() -> (Arc<RelayConfigCache>, [u8; 32]) {
        use crate::keys::SequencerKeypair;

        let keypair = SequencerKeypair::generate();
        let params = crate::types::RelayParameters {
            encryption_pubkey: Bytes::copy_from_slice(&keypair.x25519_public_key()),
            previous_encryption_pubkey: None,
            pow_difficulty: 1, // Low difficulty for tests
            attestation_pubkey: Bytes::copy_from_slice(&keypair.ed25519_public_key()),
            attestation_validity_seconds: 86400,
        };
        let config = Arc::new(RelayConfigCache::new(params));
        let secret_key = keypair.x25519_secret_bytes();

        (config, secret_key)
    }

    #[test]
    fn test_process_valid_transaction() {
        let (config, secret_key) = create_test_config_and_key();
        let submitter = Arc::new(MockSubmitter::new(false));
        let (_tx, rx) = create_receive_channel(10);

        let processor = SequencerProcessor::new(rx, secret_key, config.clone(), submitter.clone());

        // Create a valid encrypted transaction
        let encryption_pubkey = config.get().encryption_pubkey.clone();
        let pubkey_bytes: [u8; 32] = encryption_pubkey.as_ref().try_into().unwrap();
        let raw_tx_data = b"test transaction data";
        let encrypted = crypto::encrypt(&pubkey_bytes, raw_tx_data).unwrap();

        // Compute PoW
        let pow_nonce = pow::compute(&encrypted, 1);

        let msg = EncryptedTxMessage {
            request_id: 1,
            encrypted_payload: Bytes::from(encrypted),
            pow_nonce,
            encryption_pubkey,
        };

        let result = processor.process_inner(&msg);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().as_ref(), raw_tx_data);
    }

    #[test]
    fn test_process_invalid_encryption_key() {
        let (config, secret_key) = create_test_config_and_key();
        let submitter = Arc::new(MockSubmitter::new(false));
        let (_tx, rx) = create_receive_channel(10);

        let processor = SequencerProcessor::new(rx, secret_key, config, submitter);

        let msg = EncryptedTxMessage {
            request_id: 1,
            encrypted_payload: Bytes::from_static(&[1, 2, 3, 4]),
            pow_nonce: 0,
            encryption_pubkey: Bytes::from_static(&[0xff; 32]), // Wrong key
        };

        let result = processor.process_inner(&msg);
        assert!(matches!(result, Err(RelayError::InvalidEncryptionKey)));
    }

    #[test]
    fn test_process_invalid_pow() {
        use crate::keys::SequencerKeypair;

        // Create config with higher difficulty to ensure nonce 0 fails
        let keypair = SequencerKeypair::generate();
        let params = crate::types::RelayParameters {
            encryption_pubkey: Bytes::copy_from_slice(&keypair.x25519_public_key()),
            previous_encryption_pubkey: None,
            pow_difficulty: 16, // Higher difficulty - nonce 0 definitely fails
            attestation_pubkey: Bytes::copy_from_slice(&keypair.ed25519_public_key()),
            attestation_validity_seconds: 86400,
        };
        let config = Arc::new(RelayConfigCache::new(params));
        let secret_key = keypair.x25519_secret_bytes();

        let submitter = Arc::new(MockSubmitter::new(false));
        let (_tx, rx) = create_receive_channel(10);

        let processor = SequencerProcessor::new(rx, secret_key, config.clone(), submitter);

        let encryption_pubkey = config.get().encryption_pubkey.clone();

        let msg = EncryptedTxMessage {
            request_id: 1,
            encrypted_payload: Bytes::from_static(&[0xff; 100]), // Non-zero payload
            pow_nonce: 0, // Invalid nonce with difficulty 16
            encryption_pubkey,
        };

        let result = processor.process_inner(&msg);
        assert!(matches!(result, Err(RelayError::InvalidPow { .. })));
    }
}
