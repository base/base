//! Validation primitives.
//!
//! - [`AccountProofVerifier`] verifies an `eth_getProof` response against a
//!   state root using a Merkle Patricia Trie proof. Used as a guard against
//!   a compromised L2 RPC: even if the RPC lies about an account's storage
//!   hash, an invalid MPT proof exposes the lie.
//! - [`OutputValidator`] computes the expected output root at a given L2
//!   block by combining a trusted L2 header with a verified storage proof
//!   of the `L2ToL1MessagePasser` predeploy.

use std::sync::Arc;

use alloy_primitives::{Address, B256, keccak256};
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_trie::{
    Nibbles, TrieAccount,
    proof::{ProofVerificationError, verify_proof},
};
use async_trait::async_trait;
use base_common_consensus::Predeploys;
use base_proof_rpc::{L2Provider, RpcError};
use base_protocol::OutputRoot;
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

/// Errors returned by [`OutputValidator::compute_output_root`].
#[derive(Debug, Error)]
pub enum ValidatorError {
    /// The requested L2 block has not been produced yet.
    #[error("L2 block {block_number} is not yet available")]
    BlockNotAvailable {
        /// The block number that was requested.
        block_number: u64,
    },

    /// The hash returned by the RPC does not match the hash recomputed from
    /// the consensus header. Indicates a buggy or malicious RPC.
    #[error(
        "header hash mismatch at block {block_number}: rpc={rpc_hash}, computed={computed_hash}"
    )]
    HeaderHashMismatch {
        /// The block number where the mismatch was observed.
        block_number: u64,
        /// The hash returned by the RPC.
        rpc_hash: B256,
        /// The hash recomputed from the consensus header.
        computed_hash: B256,
    },

    /// The MPT account proof failed to verify against the header state root.
    #[error("account proof verification failed at block {block_number}")]
    AccountProofFailed {
        /// The block number where the proof verification failed.
        block_number: u64,
        /// The underlying [`AccountProofError`] (chained via [`std::error::Error::source`]).
        #[source]
        source: AccountProofError,
    },

    /// Underlying L2 RPC error not otherwise classified.
    #[error("L2 RPC error: {0}")]
    Rpc(#[from] RpcError),
}

/// Computes the expected output root at a given L2 block.
///
/// Implementations are expected to fetch a trusted L2 header and a verified
/// storage proof of the `L2ToL1MessagePasser` predeploy, then combine them
/// via [`OutputRoot::from_parts`].
#[async_trait]
pub trait OutputValidator: Send + Sync {
    /// Returns the output root that should appear at `block_number`
    /// according to the underlying L2 state.
    async fn compute_output_root(&self, block_number: u64) -> Result<B256, ValidatorError>;
}

/// Concrete [`OutputValidator`] backed by a real L2 RPC client.
#[derive(Debug)]
pub struct L2OutputValidator<L2: L2Provider> {
    l2: Arc<L2>,
}

impl<L2: L2Provider> L2OutputValidator<L2> {
    /// Creates a new validator wrapping the provided L2 RPC client.
    pub const fn new(l2: Arc<L2>) -> Self {
        Self { l2 }
    }
}

#[async_trait]
impl<L2: L2Provider> OutputValidator for L2OutputValidator<L2> {
    async fn compute_output_root(&self, block_number: u64) -> Result<B256, ValidatorError> {
        let rpc_header =
            self.l2.header_by_number(Some(block_number)).await.map_err(|e| match &e {
                RpcError::HeaderNotFound(_) | RpcError::BlockNotFound(_) => {
                    ValidatorError::BlockNotAvailable { block_number }
                }
                _ => ValidatorError::Rpc(e),
            })?;

        let rpc_hash = rpc_header.hash;
        let consensus = rpc_header.inner;
        let computed_hash = consensus.hash_slow();
        if rpc_hash != computed_hash {
            return Err(ValidatorError::HeaderHashMismatch {
                block_number,
                rpc_hash,
                computed_hash,
            });
        }

        let proof = self.l2.get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, rpc_hash).await?;

        AccountProofVerifier::verify(
            &proof,
            consensus.state_root,
            Predeploys::L2_TO_L1_MESSAGE_PASSER,
        )
        .map_err(|source| ValidatorError::AccountProofFailed { block_number, source })?;

        Ok(OutputRoot::from_parts(consensus.state_root, proof.storage_hash, computed_hash).hash())
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

    mod output_validator {
        use std::sync::Arc;

        use alloy_primitives::B256;
        use alloy_rpc_types_eth::Header as RpcHeader;

        use super::*;
        use crate::test_utils::{
            MockL2Provider, build_account_at_block, build_message_passer_proof,
        };

        #[tokio::test]
        async fn compute_output_root_returns_expected_root() {
            let storage_hash = B256::repeat_byte(0xBB);
            let (header, proof) = build_message_passer_proof(100, storage_hash);
            let state_root = header.state_root;
            let block_hash = header.hash_slow();
            let expected = OutputRoot::from_parts(state_root, storage_hash, block_hash).hash();

            let mut provider = MockL2Provider::new();
            provider.insert_block(100, header, proof);

            let validator = L2OutputValidator::new(Arc::new(provider));
            let got = validator.compute_output_root(100).await.expect("must succeed");

            assert_eq!(got, expected);
        }

        #[tokio::test]
        async fn compute_output_root_returns_block_not_available() {
            let mut provider = MockL2Provider::new();
            provider.error_blocks.push(999);
            let validator = L2OutputValidator::new(Arc::new(provider));

            let err = validator.compute_output_root(999).await.expect_err("must fail");

            assert!(
                matches!(err, ValidatorError::BlockNotAvailable { block_number: 999 }),
                "expected BlockNotAvailable, got {err:?}"
            );
        }

        #[tokio::test]
        async fn compute_output_root_returns_block_not_available_when_header_missing() {
            let provider = MockL2Provider::new();
            let validator = L2OutputValidator::new(Arc::new(provider));

            let err = validator.compute_output_root(42).await.expect_err("must fail");

            assert!(
                matches!(err, ValidatorError::BlockNotAvailable { block_number: 42 }),
                "expected BlockNotAvailable, got {err:?}"
            );
        }

        #[tokio::test]
        async fn compute_output_root_rejects_header_hash_mismatch() {
            let storage_hash = B256::repeat_byte(0xBB);
            let (header, proof) = build_message_passer_proof(100, storage_hash);
            let computed_hash = header.hash_slow();
            let tampered_hash = B256::repeat_byte(0xEE);

            let mut provider = MockL2Provider::new();
            provider.insert_block(100, header.clone(), proof);
            provider.headers.insert(
                100,
                RpcHeader { hash: tampered_hash, inner: header, ..Default::default() },
            );

            let validator = L2OutputValidator::new(Arc::new(provider));
            let err = validator.compute_output_root(100).await.expect_err("must fail");

            match err {
                ValidatorError::HeaderHashMismatch { block_number, rpc_hash, computed_hash: c } => {
                    assert_eq!(block_number, 100);
                    assert_eq!(rpc_hash, tampered_hash);
                    assert_eq!(c, computed_hash);
                }
                other => panic!("expected HeaderHashMismatch, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn compute_output_root_rejects_proof_for_wrong_address() {
            let storage_hash = B256::repeat_byte(0xBB);
            let other_address = address!("00000000000000000000000000000000000000ff");
            let (header, proof) =
                build_account_at_block(100, other_address, 0, U256::ZERO, storage_hash, B256::ZERO);

            let mut provider = MockL2Provider::new();
            provider.insert_block(100, header, proof);

            let validator = L2OutputValidator::new(Arc::new(provider));
            let err = validator.compute_output_root(100).await.expect_err("must fail");

            assert!(
                matches!(err, ValidatorError::AccountProofFailed { block_number: 100, .. }),
                "expected AccountProofFailed, got {err:?}"
            );
        }

        #[tokio::test]
        async fn compute_output_root_propagates_other_rpc_errors() {
            let storage_hash = B256::repeat_byte(0xBB);
            let (header, _proof) = build_message_passer_proof(100, storage_hash);
            let mut provider = MockL2Provider::new();
            let block_hash = header.hash_slow();
            provider
                .headers
                .insert(100, RpcHeader { hash: block_hash, inner: header, ..Default::default() });
            // No proof inserted under block_hash, so get_proof returns ProofNotFound.

            let validator = L2OutputValidator::new(Arc::new(provider));
            let err = validator.compute_output_root(100).await.expect_err("must fail");

            assert!(matches!(err, ValidatorError::Rpc(_)), "expected Rpc error, got {err:?}");
        }
    }
}
