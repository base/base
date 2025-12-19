//! Wire protocol messages for the encrypted relay P2P subprotocol.
//!
//! Messages are RLP-encoded for transmission over devp2p.

use alloy_primitives::{B256, Bytes};
use alloy_rlp::{RlpDecodable, RlpEncodable};

/// Protocol version identifier.
pub const PROTOCOL_VERSION: u8 = 1;

/// Protocol name for devp2p capability negotiation.
pub const PROTOCOL_NAME: &str = "encrypted-relay";

/// Message IDs for the encrypted relay protocol.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayMessageId {
    /// Submit encrypted transaction for relay (relay -> sequencer).
    EncryptedTx = 0x00,
    /// Acknowledgment with status (sequencer -> relay).
    Ack = 0x01,
    /// Identity announcement (sequencer -> relay, sent on connection).
    Identity = 0x02,
}

impl TryFrom<u8> for RelayMessageId {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::EncryptedTx),
            0x01 => Ok(Self::Ack),
            0x02 => Ok(Self::Identity),
            _ => Err(()),
        }
    }
}

/// Encrypted transaction submission message.
///
/// Sent from relay nodes to the sequencer to forward an encrypted transaction.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct EncryptedTxMessage {
    /// Request ID for correlation with acknowledgment.
    pub request_id: u64,
    /// ECIES encrypted payload.
    ///
    /// Format: `ephemeral_pubkey (32) || nonce (12) || ciphertext || tag (16)`
    pub encrypted_payload: Bytes,
    /// Proof-of-work nonce.
    pub pow_nonce: u64,
    /// The encryption public key that was used.
    pub encryption_pubkey: Bytes,
}

impl EncryptedTxMessage {
    /// Returns the message ID for this message type.
    pub const fn message_id() -> u8 {
        RelayMessageId::EncryptedTx as u8
    }
}

/// Acknowledgment message.
///
/// Sent from the sequencer back to relay nodes to confirm receipt.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct AckMessage {
    /// Request ID this acknowledges.
    pub request_id: u64,
    /// Whether the transaction was accepted.
    pub accepted: bool,
    /// Error code if not accepted (0 = success).
    pub error_code: u8,
    /// Commitment hash (SHA256 of encrypted payload).
    pub commitment: B256,
}

impl AckMessage {
    /// Returns the message ID for this message type.
    pub const fn message_id() -> u8 {
        RelayMessageId::Ack as u8
    }

    /// Creates a success acknowledgment.
    pub fn success(request_id: u64, commitment: B256) -> Self {
        Self {
            request_id,
            accepted: true,
            error_code: 0,
            commitment,
        }
    }

    /// Creates a failure acknowledgment.
    pub fn failure(request_id: u64, commitment: B256, error_code: u8) -> Self {
        Self {
            request_id,
            accepted: false,
            error_code,
            commitment,
        }
    }
}

/// Identity announcement message.
///
/// Sent by sequencers on connection establishment to identify themselves
/// to relay nodes. Contains a signed attestation that proves sequencer status.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct IdentityMessage {
    /// The node ID of the sender (should match the connected peer ID).
    pub node_id: B256,
    /// Unix timestamp when the attestation was created.
    pub timestamp: u64,
    /// Ed25519 signature over (node_id || timestamp).
    pub signature: Bytes,
}

impl IdentityMessage {
    /// Returns the message ID for this message type.
    pub const fn message_id() -> u8 {
        RelayMessageId::Identity as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rlp::{Decodable, Encodable};

    #[test]
    fn test_encrypted_tx_message_roundtrip() {
        let msg = EncryptedTxMessage {
            request_id: 12345,
            encrypted_payload: Bytes::from_static(&[1, 2, 3, 4, 5]),
            pow_nonce: 98765,
            encryption_pubkey: Bytes::from_static(&[0xab; 32]),
        };

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let decoded = EncryptedTxMessage::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_ack_message_roundtrip() {
        let msg = AckMessage::success(12345, B256::repeat_byte(0xcd));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let decoded = AckMessage::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_ack_message_failure() {
        let msg = AckMessage::failure(99, B256::ZERO, 3);

        assert!(!msg.accepted);
        assert_eq!(msg.error_code, 3);
    }

    #[test]
    fn test_message_id_conversion() {
        assert_eq!(RelayMessageId::try_from(0x00), Ok(RelayMessageId::EncryptedTx));
        assert_eq!(RelayMessageId::try_from(0x01), Ok(RelayMessageId::Ack));
        assert_eq!(RelayMessageId::try_from(0x02), Ok(RelayMessageId::Identity));
        assert_eq!(RelayMessageId::try_from(0x03), Err(()));
    }

    #[test]
    fn test_identity_message_roundtrip() {
        let msg = IdentityMessage {
            node_id: B256::repeat_byte(0x11),
            timestamp: 1234567890,
            signature: Bytes::from_static(&[0xab; 64]),
        };

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let decoded = IdentityMessage::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(msg, decoded);
    }
}
