//! Account proof verification.
//!
//! [`AccountProofVerifier`] verifies an `eth_getProof` response against a
//! state root using a Merkle Patricia Trie proof. Used as a guard against
//! a compromised L2 RPC: even if the RPC lies about an account's storage
//! hash, an invalid MPT proof exposes the lie.

use alloy_primitives::{Address, B256, keccak256};
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_trie::{
    Nibbles, TrieAccount,
    proof::{ProofVerificationError, verify_proof},
};
use thiserror::Error;

/// Errors returned by [`AccountProofVerifier::verify`].
#[derive(Debug, Eq, PartialEq, Error)]
pub enum AccountProofError {
    /// The RPC response is for a different account than the caller asked for.
    /// Indicates a buggy or malicious RPC endpoint.
    #[error("account proof address mismatch: expected {expected}, got {actual}")]
    AddressMismatch {
        /// The address the caller asked to verify.
        expected: Address,
        /// The address actually returned in the proof response.
        actual: Address,
    },

    /// The Merkle proof failed to verify against the supplied state root.
    /// Either the proof is malformed, the account fields don't match, or
    /// the state root is wrong.
    #[error("account proof verification failed: {0}")]
    VerificationFailed(#[from] ProofVerificationError),
}

/// Verifies `eth_getProof` responses against an L2 state root.
#[derive(Debug)]
pub struct AccountProofVerifier;

impl AccountProofVerifier {
    /// Verifies that `response` proves the account at `expected_address`
    /// exists in the trie rooted at `state_root` with the fields reported
    /// in the response.
    ///
    /// Returns [`AccountProofError::AddressMismatch`] if the response is
    /// for a different address than requested, or
    /// [`AccountProofError::VerificationFailed`] if the MPT proof does not
    /// validate against `state_root`.
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

        let key = Nibbles::unpack(keccak256(expected_address));

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

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bytes, U256, address, b256};
    use alloy_rpc_types_eth::EIP1186AccountProofResponse;
    use alloy_trie::{HashBuilder, Nibbles, TrieAccount, proof::ProofRetainer};

    use super::*;

    /// Builds a minimal MPT containing a single account leaf and returns
    /// the resulting state root and an `eth_getProof`-shaped response.
    fn build_account_proof(
        address: Address,
        nonce: u64,
        balance: U256,
        storage_hash: B256,
        code_hash: B256,
    ) -> (B256, EIP1186AccountProofResponse) {
        let account = TrieAccount { nonce, balance, storage_root: storage_hash, code_hash };
        let mut encoded = Vec::with_capacity(account.length());
        account.encode(&mut encoded);

        let account_key = Nibbles::unpack(keccak256(address));
        let mut hb =
            HashBuilder::default().with_proof_retainer(ProofRetainer::new(vec![account_key]));
        hb.add_leaf(account_key, &encoded);
        let state_root = hb.root();
        let proof_nodes = hb.take_proof_nodes();
        let account_proof: Vec<Bytes> =
            proof_nodes.into_nodes_sorted().into_iter().map(|(_, v)| v).collect();

        let response = EIP1186AccountProofResponse {
            address,
            account_proof,
            balance,
            code_hash,
            nonce,
            storage_hash,
            storage_proof: vec![],
        };

        (state_root, response)
    }

    const TEST_ADDRESS: Address = address!("4200000000000000000000000000000000000016");
    const TEST_STORAGE_HASH: B256 =
        b256!("1111111111111111111111111111111111111111111111111111111111111111");
    const TEST_CODE_HASH: B256 =
        b256!("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470");

    #[test]
    fn verify_valid_proof_succeeds() {
        let (state_root, response) =
            build_account_proof(TEST_ADDRESS, 0, U256::ZERO, TEST_STORAGE_HASH, TEST_CODE_HASH);

        let result = AccountProofVerifier::verify(&response, state_root, TEST_ADDRESS);

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn verify_rejects_address_mismatch() {
        let (state_root, response) =
            build_account_proof(TEST_ADDRESS, 0, U256::ZERO, TEST_STORAGE_HASH, TEST_CODE_HASH);
        let other_address = address!("0000000000000000000000000000000000000bad");

        let result = AccountProofVerifier::verify(&response, state_root, other_address);

        assert_eq!(
            result,
            Err(AccountProofError::AddressMismatch {
                expected: other_address,
                actual: TEST_ADDRESS,
            })
        );
    }

    #[test]
    fn verify_rejects_wrong_state_root() {
        let (_state_root, response) =
            build_account_proof(TEST_ADDRESS, 0, U256::ZERO, TEST_STORAGE_HASH, TEST_CODE_HASH);
        let bad_root = B256::repeat_byte(0xFF);

        let result = AccountProofVerifier::verify(&response, bad_root, TEST_ADDRESS);

        assert!(matches!(result, Err(AccountProofError::VerificationFailed(_))));
    }

    #[test]
    fn verify_rejects_tampered_account_field() {
        let (state_root, mut response) =
            build_account_proof(TEST_ADDRESS, 0, U256::ZERO, TEST_STORAGE_HASH, TEST_CODE_HASH);
        response.balance = U256::from(1u64);

        let result = AccountProofVerifier::verify(&response, state_root, TEST_ADDRESS);

        assert!(matches!(result, Err(AccountProofError::VerificationFailed(_))));
    }

    #[test]
    fn verify_rejects_empty_proof() {
        let (state_root, mut response) =
            build_account_proof(TEST_ADDRESS, 0, U256::ZERO, TEST_STORAGE_HASH, TEST_CODE_HASH);
        response.account_proof.clear();

        let result = AccountProofVerifier::verify(&response, state_root, TEST_ADDRESS);

        let err = result.expect_err("empty proof must fail");
        let AccountProofError::VerificationFailed(inner) = err else {
            panic!("expected VerificationFailed, got {err:?}");
        };
        let _ = inner;
    }
}
