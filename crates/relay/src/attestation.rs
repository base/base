//! Ed25519 attestation signing and verification for sequencer discovery.
//!
//! Attestations are used in ENR records to prove that a node is an authorized sequencer.
//! Each attestation contains:
//! - The node's ENR node ID
//! - A timestamp (for expiration)
//! - An Ed25519 signature over `keccak256(node_id || timestamp)`
//!
//! The attestation public key is stored on-chain in the `EncryptedRelayConfig` contract.

use alloy_primitives::{keccak256, B256, Bytes};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use crate::error::RelayError;
use crate::keys::SequencerKeypair;
use crate::types::RelayAttestation;

/// Creates a signed attestation for the given node ID.
///
/// The attestation proves that this node is authorized as a sequencer.
/// It should be included in the node's ENR record.
pub fn create_attestation(node_id: B256, keypair: &SequencerKeypair) -> RelayAttestation {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time before UNIX epoch")
        .as_secs();

    let mut attestation = RelayAttestation {
        node_id,
        timestamp,
        signature: Bytes::new(),
    };

    // Sign the message hash
    let msg = attestation.signing_message();
    let msg_hash = keccak256(&msg);
    let signature = keypair.ed25519_signing_key().sign(msg_hash.as_slice());

    attestation.signature = Bytes::copy_from_slice(&signature.to_bytes());
    attestation
}

/// Verifies an attestation signature against the given public key.
///
/// # Arguments
/// * `attestation` - The attestation to verify
/// * `attestation_pubkey` - The Ed25519 public key from on-chain config
///
/// # Returns
/// * `Ok(())` if the signature is valid
/// * `Err(RelayError::InvalidAttestation)` if verification fails
pub fn verify_attestation(
    attestation: &RelayAttestation,
    attestation_pubkey: &[u8; 32],
) -> Result<(), RelayError> {
    // Parse the public key
    let verifying_key = VerifyingKey::from_bytes(attestation_pubkey)
        .map_err(|e| RelayError::InvalidAttestation(format!("invalid attestation pubkey: {e}")))?;

    // Parse the signature
    let signature_bytes: [u8; 64] = attestation
        .signature
        .as_ref()
        .try_into()
        .map_err(|_| RelayError::InvalidAttestation("signature must be 64 bytes".into()))?;
    let signature = Signature::from_bytes(&signature_bytes);

    // Compute the message hash
    let msg = attestation.signing_message();
    let msg_hash = keccak256(&msg);

    // Verify
    verifying_key
        .verify(msg_hash.as_slice(), &signature)
        .map_err(|e| RelayError::InvalidAttestation(format!("signature verification failed: {e}")))
}

/// Checks if an attestation is still valid based on its timestamp.
///
/// # Arguments
/// * `attestation` - The attestation to check
/// * `validity_seconds` - How long attestations remain valid (from on-chain config)
///
/// # Returns
/// * `true` if the attestation is within the validity window
/// * `false` if the attestation has expired
pub fn is_attestation_valid(attestation: &RelayAttestation, validity_seconds: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Check if attestation is from the future (clock skew tolerance: 60 seconds)
    if attestation.timestamp > now + 60 {
        return false;
    }

    // Check if attestation has expired
    now.saturating_sub(attestation.timestamp) <= validity_seconds
}

/// Verifies an attestation including timestamp validity.
///
/// This is the main verification function that checks both:
/// 1. The Ed25519 signature is valid
/// 2. The attestation hasn't expired
///
/// # Arguments
/// * `attestation` - The attestation to verify
/// * `attestation_pubkey` - The Ed25519 public key from on-chain config
/// * `validity_seconds` - How long attestations remain valid
pub fn verify_attestation_full(
    attestation: &RelayAttestation,
    attestation_pubkey: &[u8; 32],
    validity_seconds: u64,
) -> Result<(), RelayError> {
    // Check timestamp first (cheaper than signature verification)
    if !is_attestation_valid(attestation, validity_seconds) {
        return Err(RelayError::InvalidAttestation("attestation has expired".into()));
    }

    // Verify signature
    verify_attestation(attestation, attestation_pubkey)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::SequencerKeypair;

    fn random_node_id() -> B256 {
        use sha2::{Sha256, Digest};
        let mut hasher = Sha256::new();
        hasher.update(rand::random::<[u8; 32]>());
        B256::from_slice(&hasher.finalize())
    }

    #[test]
    fn test_create_and_verify_attestation() {
        let keypair = SequencerKeypair::generate();
        let node_id = random_node_id();

        let attestation = create_attestation(node_id, &keypair);

        assert_eq!(attestation.node_id, node_id);
        assert!(!attestation.signature.is_empty());
        assert_eq!(attestation.signature.len(), 64);

        // Verify should succeed with correct pubkey
        let pubkey = keypair.ed25519_public_key();
        assert!(verify_attestation(&attestation, &pubkey).is_ok());
    }

    #[test]
    fn test_verify_attestation_wrong_pubkey() {
        let keypair = SequencerKeypair::generate();
        let other_keypair = SequencerKeypair::generate();
        let node_id = random_node_id();

        let attestation = create_attestation(node_id, &keypair);

        // Verify should fail with wrong pubkey
        let wrong_pubkey = other_keypair.ed25519_public_key();
        assert!(verify_attestation(&attestation, &wrong_pubkey).is_err());
    }

    #[test]
    fn test_verify_attestation_tampered_node_id() {
        let keypair = SequencerKeypair::generate();
        let node_id = random_node_id();

        let mut attestation = create_attestation(node_id, &keypair);

        // Tamper with node_id
        attestation.node_id = random_node_id();

        // Verify should fail
        let pubkey = keypair.ed25519_public_key();
        assert!(verify_attestation(&attestation, &pubkey).is_err());
    }

    #[test]
    fn test_verify_attestation_tampered_timestamp() {
        let keypair = SequencerKeypair::generate();
        let node_id = random_node_id();

        let mut attestation = create_attestation(node_id, &keypair);

        // Tamper with timestamp
        attestation.timestamp += 1;

        // Verify should fail
        let pubkey = keypair.ed25519_public_key();
        assert!(verify_attestation(&attestation, &pubkey).is_err());
    }

    #[test]
    fn test_attestation_validity_fresh() {
        let keypair = SequencerKeypair::generate();
        let node_id = random_node_id();

        let attestation = create_attestation(node_id, &keypair);

        // Fresh attestation should be valid
        assert!(is_attestation_valid(&attestation, 86400)); // 24 hours
        assert!(is_attestation_valid(&attestation, 60));    // 1 minute
    }

    #[test]
    fn test_attestation_validity_expired() {
        let keypair = SequencerKeypair::generate();
        let node_id = random_node_id();

        let mut attestation = create_attestation(node_id, &keypair);

        // Set timestamp to 2 hours ago
        attestation.timestamp -= 7200;

        // Should be valid with 24-hour window
        assert!(is_attestation_valid(&attestation, 86400));

        // Should be invalid with 1-hour window
        assert!(!is_attestation_valid(&attestation, 3600));
    }

    #[test]
    fn test_attestation_validity_future() {
        let keypair = SequencerKeypair::generate();
        let node_id = random_node_id();

        let mut attestation = create_attestation(node_id, &keypair);

        // Set timestamp to far in the future (beyond 60s tolerance)
        attestation.timestamp += 120;

        // Should be invalid
        assert!(!is_attestation_valid(&attestation, 86400));
    }

    #[test]
    fn test_verify_attestation_full() {
        let keypair = SequencerKeypair::generate();
        let node_id = random_node_id();
        let pubkey = keypair.ed25519_public_key();

        let attestation = create_attestation(node_id, &keypair);

        // Should succeed with valid attestation
        assert!(verify_attestation_full(&attestation, &pubkey, 86400).is_ok());
    }

    #[test]
    fn test_verify_attestation_full_expired() {
        let keypair = SequencerKeypair::generate();
        let node_id = random_node_id();
        let pubkey = keypair.ed25519_public_key();

        let mut attestation = create_attestation(node_id, &keypair);
        attestation.timestamp -= 7200; // 2 hours ago

        // Should fail with 1-hour validity
        let result = verify_attestation_full(&attestation, &pubkey, 3600);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("expired"));
    }
}
