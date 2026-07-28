//! Host-side recovery of the enclave signer from a generated TEE proof.
//!
//! The enclave signs each per-block [`Proposal`] over the keccak256 of its
//! encoded [`ProofJournal`]. Rather than querying the enclave for its signer in
//! a separate round-trip (which could observe a different key after a restart),
//! the host recovers the signer directly from the proof's own signature. The
//! recovered address is therefore guaranteed to be the exact key that signed
//! the proof.

use alloy_primitives::{Address, B256, keccak256};
use alloy_signer::utils::public_key_to_address;
use base_proof_primitives::{ECDSA_SIGNATURE_LENGTH, ProofJournal, Proposal};
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

use crate::NitroHostError;

/// Recovers the enclave signer address from a signed proposal.
#[derive(Debug)]
pub struct TeeSignerRecovery;

impl TeeSignerRecovery {
    /// Recover the signer of a per-block [`Proposal`].
    ///
    /// `proposer` and `tee_image_hash` are the two [`ProofJournal`] fields not
    /// carried on the proposal; the host supplies them from the originating
    /// proof request. The proposal must be a per-block proposal (empty
    /// intermediate roots), matching how the enclave signs each block.
    ///
    /// # Errors
    ///
    /// Returns [`NitroHostError::SignerRecovery`] if the block number is zero,
    /// the signature is malformed, or the public key cannot be recovered.
    pub fn recover_from_proposal(
        proposal: &Proposal,
        proposer: Address,
        tee_image_hash: B256,
    ) -> Result<Address, NitroHostError> {
        let starting_l2_block = proposal.l2_block_number.checked_sub(1).ok_or_else(|| {
            NitroHostError::SignerRecovery("proposal l2_block_number is 0".to_owned())
        })?;

        let journal = ProofJournal {
            proposer,
            l1_origin_hash: proposal.l1_origin_hash,
            prev_output_root: proposal.prev_output_root,
            starting_l2_block,
            output_root: proposal.output_root,
            ending_l2_block: proposal.l2_block_number,
            intermediate_roots: Vec::new(),
            config_hash: proposal.config_hash,
            tee_image_hash,
        };
        let digest = keccak256(journal.encode());

        if proposal.signature.len() != ECDSA_SIGNATURE_LENGTH {
            return Err(NitroHostError::SignerRecovery(format!(
                "expected {ECDSA_SIGNATURE_LENGTH}-byte signature, got {}",
                proposal.signature.len()
            )));
        }
        let signature = Signature::from_slice(&proposal.signature[..64])
            .map_err(|e| NitroHostError::SignerRecovery(format!("invalid signature: {e}")))?;
        let recovery_id = RecoveryId::from_byte(proposal.signature[64]).ok_or_else(|| {
            NitroHostError::SignerRecovery(format!(
                "invalid recovery id {}",
                proposal.signature[64]
            ))
        })?;

        let verifying_key =
            VerifyingKey::recover_from_prehash(digest.as_slice(), &signature, recovery_id)
                .map_err(|e| NitroHostError::SignerRecovery(format!("recovery failed: {e}")))?;
        Ok(public_key_to_address(&verifying_key))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;

    use super::*;

    fn signed_proposal(
        signer: &PrivateKeySigner,
        proposer: Address,
        tee_image_hash: B256,
    ) -> Proposal {
        let mut proposal = Proposal {
            output_root: B256::repeat_byte(0xaa),
            signature: Bytes::new(),
            l1_origin_hash: B256::repeat_byte(0xbb),
            l1_origin_number: 100,
            l2_block_number: 42,
            prev_output_root: B256::repeat_byte(0xcc),
            config_hash: B256::repeat_byte(0xdd),
        };

        let journal = ProofJournal {
            proposer,
            l1_origin_hash: proposal.l1_origin_hash,
            prev_output_root: proposal.prev_output_root,
            starting_l2_block: proposal.l2_block_number - 1,
            output_root: proposal.output_root,
            ending_l2_block: proposal.l2_block_number,
            intermediate_roots: Vec::new(),
            config_hash: proposal.config_hash,
            tee_image_hash,
        };
        let signature = signer.sign_hash_sync(&keccak256(journal.encode())).unwrap();
        proposal.signature = Bytes::from(signature.as_rsy().to_vec());
        proposal
    }

    #[test]
    fn recovers_the_signing_address() {
        let signer = PrivateKeySigner::random();
        let proposer = Address::repeat_byte(0x11);
        let tee_image_hash = B256::repeat_byte(0x22);

        let proposal = signed_proposal(&signer, proposer, tee_image_hash);
        let recovered =
            TeeSignerRecovery::recover_from_proposal(&proposal, proposer, tee_image_hash).unwrap();

        assert_eq!(recovered, signer.address());
    }

    #[test]
    fn wrong_context_recovers_a_different_address() {
        let signer = PrivateKeySigner::random();
        let proposer = Address::repeat_byte(0x11);
        let tee_image_hash = B256::repeat_byte(0x22);

        let proposal = signed_proposal(&signer, proposer, tee_image_hash);
        // Recovering with a mismatched image hash yields some other address, never the signer.
        let recovered =
            TeeSignerRecovery::recover_from_proposal(&proposal, proposer, B256::repeat_byte(0x33))
                .unwrap();

        assert_ne!(recovered, signer.address());
    }

    #[test]
    fn malformed_signature_is_rejected() {
        let proposal = Proposal {
            output_root: B256::ZERO,
            signature: Bytes::from(vec![0u8; 10]),
            l1_origin_hash: B256::ZERO,
            l1_origin_number: 0,
            l2_block_number: 1,
            prev_output_root: B256::ZERO,
            config_hash: B256::ZERO,
        };

        assert!(matches!(
            TeeSignerRecovery::recover_from_proposal(&proposal, Address::ZERO, B256::ZERO),
            Err(NitroHostError::SignerRecovery(_))
        ));
    }

    #[test]
    fn zero_block_number_is_rejected_with_specific_error() {
        let proposal = Proposal {
            output_root: B256::ZERO,
            signature: Bytes::from(vec![0u8; ECDSA_SIGNATURE_LENGTH]),
            l1_origin_hash: B256::ZERO,
            l1_origin_number: 0,
            l2_block_number: 0,
            prev_output_root: B256::ZERO,
            config_hash: B256::ZERO,
        };

        let err = TeeSignerRecovery::recover_from_proposal(&proposal, Address::ZERO, B256::ZERO)
            .unwrap_err();
        assert!(
            matches!(&err, NitroHostError::SignerRecovery(msg) if msg.contains("l2_block_number is 0"))
        );
    }
}
