//! Core types for the encrypted transaction relay.

use alloy_primitives::{B256, Bytes};
use serde::{Deserialize, Serialize};

/// Request to submit an encrypted transaction for relay to the sequencer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptedTransactionRequest {
    /// ECIES encrypted transaction payload.
    ///
    /// Format: `ephemeral_pubkey (32 bytes) || nonce (12 bytes) || ciphertext || tag (16 bytes)`
    pub encrypted_payload: Bytes,

    /// Proof-of-work nonce that satisfies the current difficulty requirement.
    pub pow_nonce: u64,

    /// The encryption public key used (32 bytes).
    ///
    /// Sequencer validates this matches current or previous key.
    pub encryption_pubkey: Bytes,
}

/// Response from submitting an encrypted transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptedTransactionResponse {
    /// Whether the relay accepted the transaction for forwarding.
    pub accepted: bool,

    /// Commitment hash for tracking (SHA256 of encrypted_payload).
    pub commitment: B256,

    /// Error message if not accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Current relay parameters fetched from on-chain config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayParameters {
    /// Current X25519 encryption public key (32 bytes).
    pub encryption_pubkey: Bytes,

    /// Previous encryption public key (for grace period during rotation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_encryption_pubkey: Option<Bytes>,

    /// Current proof-of-work difficulty (number of leading zero bits required).
    pub pow_difficulty: u8,

    /// Ed25519 public key used to verify sequencer node attestations (32 bytes).
    pub attestation_pubkey: Bytes,

    /// How long attestations remain valid (seconds).
    pub attestation_validity_seconds: u64,
}

/// Attestation that proves a node is a valid sequencer.
///
/// Included in ENR records for sequencer discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayAttestation {
    /// The node ID this attestation is for.
    pub node_id: B256,

    /// Unix timestamp when this attestation was issued.
    pub timestamp: u64,

    /// Ed25519 signature over `keccak256(node_id || timestamp)`.
    pub signature: Bytes,
}

impl RelayAttestation {
    /// Returns the message bytes to be hashed and signed.
    pub fn signing_message(&self) -> Vec<u8> {
        let mut msg = Vec::with_capacity(32 + 8);
        msg.extend_from_slice(self.node_id.as_slice());
        msg.extend_from_slice(&self.timestamp.to_le_bytes());
        msg
    }
}

/// Error codes for relay operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum RelayErrorCode {
    /// No error.
    Success = 0,
    /// Proof-of-work verification failed.
    InvalidPow = 1,
    /// Request was rate limited.
    RateLimited = 2,
    /// Unrecognized encryption pubkey.
    InvalidEncryptionKey = 3,
    /// Payload too large.
    PayloadTooLarge = 4,
    /// Payload too small (malformed).
    PayloadTooSmall = 5,
    /// Sequencer connection failed.
    SequencerUnavailable = 6,
    /// Internal error.
    InternalError = 255,
}

impl std::fmt::Display for RelayErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success => write!(f, "success"),
            Self::InvalidPow => write!(f, "invalid proof-of-work"),
            Self::RateLimited => write!(f, "rate limited"),
            Self::InvalidEncryptionKey => write!(f, "invalid encryption key"),
            Self::PayloadTooLarge => write!(f, "payload too large"),
            Self::PayloadTooSmall => write!(f, "payload too small"),
            Self::SequencerUnavailable => write!(f, "sequencer unavailable"),
            Self::InternalError => write!(f, "internal error"),
        }
    }
}

/// Maximum encrypted payload size (1 MB).
pub const MAX_ENCRYPTED_PAYLOAD_SIZE: usize = 1024 * 1024;

/// Minimum encrypted payload size (ephemeral key + nonce + tag + 1 byte ciphertext).
pub const MIN_ENCRYPTED_PAYLOAD_SIZE: usize = 32 + 12 + 16 + 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_attestation_signing_message() {
        let attestation = RelayAttestation {
            node_id: B256::ZERO,
            timestamp: 1234567890,
            signature: Bytes::new(),
        };

        let msg = attestation.signing_message();
        assert_eq!(msg.len(), 32 + 8);
    }

    #[test]
    fn test_request_serialization() {
        let req = EncryptedTransactionRequest {
            encrypted_payload: Bytes::from_static(&[1, 2, 3, 4]),
            pow_nonce: 12345,
            encryption_pubkey: Bytes::from_static(&[0u8; 32]),
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("encryptedPayload"));
        assert!(json.contains("powNonce"));
        assert!(json.contains("encryptionPubkey"));

        let parsed: EncryptedTransactionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pow_nonce, 12345);
    }

    #[test]
    fn test_response_serialization() {
        let resp = EncryptedTransactionResponse {
            accepted: true,
            commitment: B256::ZERO,
            error: None,
        };

        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("error")); // skip_serializing_if works

        let resp_with_error = EncryptedTransactionResponse {
            accepted: false,
            commitment: B256::ZERO,
            error: Some("test error".to_string()),
        };

        let json = serde_json::to_string(&resp_with_error).unwrap();
        assert!(json.contains("error"));
    }
}
