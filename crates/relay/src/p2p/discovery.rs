//! ENR-based sequencer discovery using attestations.
//!
//! Sequencer nodes include a signed attestation in their ENR records.
//! Relay nodes validate these attestations to identify sequencer peers.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{B256, Bytes};
use parking_lot::RwLock;

use crate::attestation::{is_attestation_valid, verify_attestation};
use crate::config::RelayConfigCache;
use crate::error::RelayError;
use crate::types::RelayAttestation;

/// ENR key for relay attestation data.
pub const ENR_ATTESTATION_KEY: &str = "relay-attest";

/// A peer identifier (32-byte node ID used in attestations).
pub type PeerId = B256;

/// A connection peer identifier (64-byte RLPx peer ID).
/// This is stored as raw bytes since reth_network_api::PeerId is 64 bytes.
pub type ConnectionPeerId = [u8; 64];

/// Entry for a tracked sequencer peer.
#[derive(Debug, Clone)]
struct SequencerEntry {
    /// The 32-byte node ID from the attestation.
    node_id: PeerId,
    /// The 64-byte connection peer ID for network operations.
    connection_peer_id: ConnectionPeerId,
}

/// Tracks peers that have been validated as sequencers.
#[derive(Debug)]
pub struct SequencerPeerTracker {
    /// Map from node_id to sequencer entry (includes connection peer ID).
    sequencers: RwLock<HashMap<PeerId, SequencerEntry>>,
    /// Config cache for attestation verification.
    config: Arc<RelayConfigCache>,
}

impl SequencerPeerTracker {
    /// Creates a new peer tracker with the given config cache.
    pub fn new(config: Arc<RelayConfigCache>) -> Self {
        Self {
            sequencers: RwLock::new(HashMap::new()),
            config,
        }
    }

    /// Validates an attestation and adds the peer if valid.
    ///
    /// The `connection_peer_id` is the 64-byte RLPx peer ID from the network layer.
    /// The `attestation.node_id` is the 32-byte node ID used in the attestation.
    ///
    /// Returns `true` if the peer is now tracked as a sequencer.
    pub fn validate_and_add_peer_with_connection(
        &self,
        connection_peer_id: &[u8],
        attestation: &RelayAttestation,
    ) -> Result<bool, RelayError> {
        // Get config parameters
        let params = self.config.get();
        let attestation_pubkey: [u8; 32] = params
            .attestation_pubkey
            .as_ref()
            .try_into()
            .map_err(|_| RelayError::InvalidAttestation("invalid attestation pubkey length".into()))?;

        // Check timestamp validity
        if !is_attestation_valid(attestation, params.attestation_validity_seconds) {
            return Err(RelayError::InvalidAttestation("attestation has expired".into()));
        }

        // Verify signature
        verify_attestation(attestation, &attestation_pubkey)?;

        // Convert connection_peer_id to fixed-size array
        let conn_id: ConnectionPeerId = connection_peer_id
            .try_into()
            .map_err(|_| RelayError::InvalidAttestation("connection peer ID must be 64 bytes".into()))?;

        // Add to tracked sequencers
        let entry = SequencerEntry {
            node_id: attestation.node_id,
            connection_peer_id: conn_id,
        };
        self.sequencers.write().insert(attestation.node_id, entry);
        Ok(true)
    }

    /// Validates an attestation and adds the peer if valid (legacy API for tests).
    ///
    /// This version uses the node_id as both the attestation ID and a placeholder
    /// for the connection ID (padded with zeros).
    ///
    /// Returns `true` if the peer is now tracked as a sequencer.
    pub fn validate_and_add_peer(
        &self,
        peer_id: PeerId,
        attestation: &RelayAttestation,
    ) -> Result<bool, RelayError> {
        // Verify the attestation matches this peer
        if attestation.node_id != peer_id {
            return Err(RelayError::InvalidAttestation(
                "attestation node_id does not match peer_id".into(),
            ));
        }

        // Get config parameters
        let params = self.config.get();
        let attestation_pubkey: [u8; 32] = params
            .attestation_pubkey
            .as_ref()
            .try_into()
            .map_err(|_| RelayError::InvalidAttestation("invalid attestation pubkey length".into()))?;

        // Check timestamp validity
        if !is_attestation_valid(attestation, params.attestation_validity_seconds) {
            return Err(RelayError::InvalidAttestation("attestation has expired".into()));
        }

        // Verify signature
        verify_attestation(attestation, &attestation_pubkey)?;

        // For test compatibility, create a fake 64-byte connection ID by doubling the 32-byte node_id
        let mut conn_id = [0u8; 64];
        conn_id[..32].copy_from_slice(peer_id.as_slice());
        conn_id[32..].copy_from_slice(peer_id.as_slice());

        let entry = SequencerEntry {
            node_id: peer_id,
            connection_peer_id: conn_id,
        };
        self.sequencers.write().insert(peer_id, entry);
        Ok(true)
    }

    /// Removes a peer from tracking by node_id (e.g., on disconnect).
    pub fn remove_peer(&self, peer_id: &PeerId) {
        self.sequencers.write().remove(peer_id);
    }

    /// Removes a peer from tracking by connection peer ID.
    pub fn remove_peer_by_connection(&self, connection_peer_id: &[u8]) {
        self.sequencers.write().retain(|_, entry| {
            entry.connection_peer_id.as_slice() != connection_peer_id
        });
    }

    /// Checks if a peer is tracked as a sequencer.
    pub fn is_sequencer(&self, peer_id: &PeerId) -> bool {
        self.sequencers.read().contains_key(peer_id)
    }

    /// Returns a list of all known sequencer node IDs.
    pub fn sequencer_peers(&self) -> Vec<PeerId> {
        self.sequencers.read().keys().copied().collect()
    }

    /// Returns a list of all known sequencer connection peer IDs (64 bytes each).
    pub fn sequencer_connection_peers(&self) -> Vec<ConnectionPeerId> {
        self.sequencers.read().values().map(|e| e.connection_peer_id).collect()
    }

    /// Returns the number of tracked sequencers.
    pub fn sequencer_count(&self) -> usize {
        self.sequencers.read().len()
    }

    /// Clears all tracked sequencers.
    pub fn clear(&self) {
        self.sequencers.write().clear();
    }
}

/// Encodes a `RelayAttestation` for inclusion in an ENR record.
///
/// Format: RLP([node_id, timestamp, signature])
pub fn encode_enr_attestation(attestation: &RelayAttestation) -> Vec<u8> {
    use alloy_rlp::Encodable;

    // We encode as a tuple: (node_id, timestamp, signature)
    #[derive(alloy_rlp::RlpEncodable)]
    struct AttestationEncode<'a> {
        node_id: &'a B256,
        timestamp: u64,
        signature: &'a Bytes,
    }

    let encode = AttestationEncode {
        node_id: &attestation.node_id,
        timestamp: attestation.timestamp,
        signature: &attestation.signature,
    };

    let mut buf = Vec::new();
    encode.encode(&mut buf);
    buf
}

/// Decodes a `RelayAttestation` from ENR record bytes.
pub fn decode_enr_attestation(data: &[u8]) -> Option<RelayAttestation> {
    use alloy_rlp::Decodable;

    #[derive(alloy_rlp::RlpDecodable)]
    struct AttestationDecode {
        node_id: B256,
        timestamp: u64,
        signature: Bytes,
    }

    let decoded = AttestationDecode::decode(&mut &data[..]).ok()?;

    Some(RelayAttestation {
        node_id: decoded.node_id,
        timestamp: decoded.timestamp,
        signature: decoded.signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::SequencerKeypair;
    use crate::attestation::create_attestation;

    fn create_test_config() -> Arc<RelayConfigCache> {
        let keypair = SequencerKeypair::generate();
        let attestation_pubkey = keypair.ed25519_public_key();
        let encryption_pubkey = keypair.x25519_public_key();

        // Create a config with test keys
        let params = crate::types::RelayParameters {
            encryption_pubkey: Bytes::copy_from_slice(&encryption_pubkey),
            previous_encryption_pubkey: None,
            pow_difficulty: 18,
            attestation_pubkey: Bytes::copy_from_slice(&attestation_pubkey),
            attestation_validity_seconds: 86400,
        };

        Arc::new(RelayConfigCache::new(params))
    }

    #[test]
    fn test_attestation_encode_decode_roundtrip() {
        let keypair = SequencerKeypair::generate();
        let node_id = B256::repeat_byte(0x42);
        let attestation = create_attestation(node_id, &keypair);

        let encoded = encode_enr_attestation(&attestation);
        let decoded = decode_enr_attestation(&encoded).unwrap();

        assert_eq!(decoded.node_id, attestation.node_id);
        assert_eq!(decoded.timestamp, attestation.timestamp);
        assert_eq!(decoded.signature, attestation.signature);
    }

    #[test]
    fn test_peer_tracker_add_remove() {
        let keypair = SequencerKeypair::generate();

        // Create config with this keypair's attestation pubkey
        let params = crate::types::RelayParameters {
            encryption_pubkey: Bytes::copy_from_slice(&keypair.x25519_public_key()),
            previous_encryption_pubkey: None,
            pow_difficulty: 18,
            attestation_pubkey: Bytes::copy_from_slice(&keypair.ed25519_public_key()),
            attestation_validity_seconds: 86400,
        };
        let config = Arc::new(RelayConfigCache::new(params));

        let tracker = SequencerPeerTracker::new(config);
        let peer_id = B256::repeat_byte(0x11);
        let attestation = create_attestation(peer_id, &keypair);

        // Initially no sequencers
        assert_eq!(tracker.sequencer_count(), 0);
        assert!(!tracker.is_sequencer(&peer_id));

        // Add peer
        let result = tracker.validate_and_add_peer(peer_id, &attestation);
        assert!(result.is_ok());
        assert!(result.unwrap());
        assert_eq!(tracker.sequencer_count(), 1);
        assert!(tracker.is_sequencer(&peer_id));

        // Remove peer
        tracker.remove_peer(&peer_id);
        assert_eq!(tracker.sequencer_count(), 0);
        assert!(!tracker.is_sequencer(&peer_id));
    }

    #[test]
    fn test_peer_tracker_wrong_node_id() {
        let keypair = SequencerKeypair::generate();

        let params = crate::types::RelayParameters {
            encryption_pubkey: Bytes::copy_from_slice(&keypair.x25519_public_key()),
            previous_encryption_pubkey: None,
            pow_difficulty: 18,
            attestation_pubkey: Bytes::copy_from_slice(&keypair.ed25519_public_key()),
            attestation_validity_seconds: 86400,
        };
        let config = Arc::new(RelayConfigCache::new(params));

        let tracker = SequencerPeerTracker::new(config);

        // Create attestation for a different node_id
        let attestation_node_id = B256::repeat_byte(0x22);
        let peer_id = B256::repeat_byte(0x33);
        let attestation = create_attestation(attestation_node_id, &keypair);

        // Should fail because peer_id doesn't match attestation
        let result = tracker.validate_and_add_peer(peer_id, &attestation);
        assert!(result.is_err());
    }

    #[test]
    fn test_peer_tracker_wrong_pubkey() {
        let keypair1 = SequencerKeypair::generate();
        let keypair2 = SequencerKeypair::generate();

        // Config has keypair1's pubkey
        let params = crate::types::RelayParameters {
            encryption_pubkey: Bytes::copy_from_slice(&keypair1.x25519_public_key()),
            previous_encryption_pubkey: None,
            pow_difficulty: 18,
            attestation_pubkey: Bytes::copy_from_slice(&keypair1.ed25519_public_key()),
            attestation_validity_seconds: 86400,
        };
        let config = Arc::new(RelayConfigCache::new(params));

        let tracker = SequencerPeerTracker::new(config);
        let peer_id = B256::repeat_byte(0x44);

        // Attestation signed by keypair2
        let attestation = create_attestation(peer_id, &keypair2);

        // Should fail because signature doesn't match config's pubkey
        let result = tracker.validate_and_add_peer(peer_id, &attestation);
        assert!(result.is_err());
    }
}
