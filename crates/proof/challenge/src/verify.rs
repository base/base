//! Account proof verification utilities.
//!
//! Provides [`verify_account_proof`] for verifying `eth_getProof` responses
//! against a state root using Merkle Patricia Trie proofs.

use alloy_primitives::{B256, keccak256};
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_trie::{Nibbles, TrieAccount, proof::verify_proof};
use thiserror::Error;

/// Errors from account proof verification.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum AccountProofError {
    /// The Merkle proof does not match the expected account state.
    #[error("account proof verification failed: {0}")]
    VerificationFailed(String),
}

/// Verifies an `eth_getProof` response against a state root.
///
/// Checks that the account proof is valid against the given `state_root`
/// by RLP-encoding the account fields (nonce, balance, storage root, code
/// hash) and verifying the Merkle Patricia Trie proof.
///
/// # Errors
///
/// Returns [`AccountProofError::VerificationFailed`] if:
/// - The proof is invalid against the state root
/// - The account data doesn't match the proof
pub fn verify_account_proof(
    response: &EIP1186AccountProofResponse,
    state_root: B256,
) -> Result<(), AccountProofError> {
    let key = Nibbles::unpack(keccak256(response.address));

    let account = TrieAccount {
        nonce: response.nonce,
        balance: response.balance,
        storage_root: response.storage_hash,
        code_hash: response.code_hash,
    };

    let mut expected_value = Vec::new();
    alloy_rlp::Encodable::encode(&account, &mut expected_value);

    verify_proof(state_root, key, Some(expected_value), &response.account_proof)
        .map_err(|e| AccountProofError::VerificationFailed(e.to_string()))
}
