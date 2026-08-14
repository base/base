//! Merkle Patricia Trie proof verification utilities.

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_trie::{
    Nibbles, TrieAccount,
    proof::{ProofVerificationError, verify_proof},
};
use thiserror::Error;

/// Errors from account proof verification.
#[derive(Debug, Eq, PartialEq, Error)]
pub enum AccountProofError {
    /// The RPC response is for a different account than the caller requested.
    #[error("account proof address mismatch: expected {expected}, got {actual}")]
    AddressMismatch {
        /// The address the caller expected to verify.
        expected: Address,
        /// The address returned in the proof response.
        actual: Address,
    },
    /// The Merkle proof does not match the expected account state.
    #[error("account proof verification failed: {0}")]
    VerificationFailed(#[from] ProofVerificationError),
}

/// Errors from storage proof verification.
#[derive(Debug, Eq, PartialEq, Error)]
pub enum StorageProofError {
    /// The Merkle proof does not match the expected storage value.
    #[error("storage proof verification failed: {0}")]
    VerificationFailed(#[from] ProofVerificationError),
}

/// Verifies `eth_getProof` account responses against state roots.
#[derive(Debug)]
pub struct AccountProofVerifier;

impl AccountProofVerifier {
    /// Verify an `eth_getProof` response against a state root.
    ///
    /// # Errors
    ///
    /// Returns an error when the response address differs from `expected_address`
    /// or its account proof does not match `state_root`.
    pub fn verify(
        response: &EIP1186AccountProofResponse,
        state_root: B256,
        expected_address: Address,
    ) -> Result<(), AccountProofError> {
        if response.address != expected_address {
            return Err(AccountProofError::AddressMismatch {
                expected: expected_address,
                actual: response.address,
            });
        }

        let account = TrieAccount {
            nonce: response.nonce,
            balance: response.balance,
            storage_root: response.storage_hash,
            code_hash: response.code_hash,
        };
        let mut encoded = Vec::with_capacity(account.length());
        account.encode(&mut encoded);

        verify_proof(
            state_root,
            Nibbles::unpack(keccak256(expected_address)),
            Some(encoded),
            &response.account_proof,
        )?;
        Ok(())
    }
}

/// Verifies storage slots in Ethereum's secure storage trie.
#[derive(Debug)]
pub struct StorageProofVerifier;

impl StorageProofVerifier {
    /// Verify that `slot` contains `expected_rlp_value` under `storage_root`.
    ///
    /// # Errors
    ///
    /// Returns an error if the proof does not establish the expected value.
    pub fn verify_storage_slot(
        storage_root: B256,
        slot: B256,
        expected_rlp_value: &[u8],
        proof: &[Bytes],
    ) -> Result<(), StorageProofError> {
        verify_proof(
            storage_root,
            Nibbles::unpack(keccak256(slot)),
            Some(expected_rlp_value.to_vec()),
            proof,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_primitives::b256;
    use alloy_trie::{HashBuilder, proof::ProofRetainer};

    use super::*;

    fn storage_proof(slot: B256, value: &[u8]) -> (B256, Vec<Bytes>) {
        let key = Nibbles::unpack(keccak256(slot));
        let mut builder = HashBuilder::default().with_proof_retainer(ProofRetainer::new(vec![key]));
        builder.add_leaf(key, value);
        let storage_root = builder.root();
        let proof = builder
            .take_proof_nodes()
            .into_nodes_sorted()
            .into_iter()
            .map(|(_, node)| node)
            .collect();
        (storage_root, proof)
    }

    #[test]
    fn verifies_storage_slot() {
        let slot = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let (storage_root, proof) = storage_proof(slot, &[0x01]);

        assert!(
            StorageProofVerifier::verify_storage_slot(storage_root, slot, &[0x01], &proof).is_ok()
        );
        assert!(
            StorageProofVerifier::verify_storage_slot(storage_root, slot, &[0x02], &proof).is_err()
        );
        assert!(
            StorageProofVerifier::verify_storage_slot(B256::ZERO, slot, &[0x01], &proof).is_err()
        );
    }
}
