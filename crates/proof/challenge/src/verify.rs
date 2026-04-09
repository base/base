//! Account proof verification utilities.
//!
//! Provides [`AccountProofVerifier`] for verifying `eth_getProof` responses
//! against a state root using Merkle Patricia Trie proofs.

use alloy_primitives::{B256, keccak256};
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
    /// The Merkle proof does not match the expected account state.
    #[error("account proof verification failed: {0}")]
    VerificationFailed(#[from] ProofVerificationError),
}

/// Verifies `eth_getProof` responses against state roots.
#[derive(Debug)]
pub struct AccountProofVerifier;

impl AccountProofVerifier {
    /// Verifies an `eth_getProof` response against a state root.
    ///
    /// # Errors
    ///
    /// Returns [`AccountProofError::VerificationFailed`] if the proof is
    /// invalid or the account data doesn't match.
    pub fn verify(
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

        let mut encoded = Vec::with_capacity(account.length());
        account.encode(&mut encoded);

        verify_proof(state_root, key, Some(encoded), &response.account_proof)?;
        Ok(())
    }
}
