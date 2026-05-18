//! L2 output root computation.
//!
//! [`OutputValidator`] computes the expected output root at a given L2
//! block by combining a trusted L2 header with a verified storage proof
//! of the `L2ToL1MessagePasser` predeploy. [`L2OutputValidator`] is the
//! concrete implementation backed by an `L2Provider` and an
//! [`AccountProofVerifier`].

use std::sync::Arc;

use alloy_primitives::B256;
use async_trait::async_trait;
use base_common_consensus::Predeploys;
use base_proof_rpc::{L2Provider, RpcError};
use base_protocol::OutputRoot;
use thiserror::Error;

use super::account_proof::{AccountProofError, AccountProofVerifier};

/// Errors returned by [`OutputValidator::compute_output_root`].
#[derive(Debug, Error)]
pub enum OutputRootError {
    /// The requested L2 block has not been produced yet.
    #[error("L2 block {block_number} is not yet available")]
    BlockNotAvailable {
        /// The block number that was requested.
        block_number: u64,
        /// The underlying [`RpcError`] (chained via [`std::error::Error::source`]).
        #[source]
        source: RpcError,
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
    async fn compute_output_root(&self, block_number: u64) -> Result<B256, OutputRootError>;
}

/// Concrete [`OutputValidator`] backed by a real L2 RPC client.
#[derive(Debug)]
pub struct L2OutputValidator<L2: L2Provider + ?Sized> {
    l2: Arc<L2>,
}

impl<L2: L2Provider + ?Sized> L2OutputValidator<L2> {
    /// Creates a new validator wrapping the provided L2 RPC client.
    pub const fn new(l2: Arc<L2>) -> Self {
        Self { l2 }
    }
}

#[async_trait]
impl<L2: L2Provider + ?Sized> OutputValidator for L2OutputValidator<L2> {
    async fn compute_output_root(&self, block_number: u64) -> Result<B256, OutputRootError> {
        let rpc_header =
            self.l2.header_by_number(Some(block_number)).await.map_err(|e| match e {
                e @ (RpcError::HeaderNotFound(_) | RpcError::BlockNotFound(_)) => {
                    OutputRootError::BlockNotAvailable { block_number, source: e }
                }
                e => OutputRootError::Rpc(e),
            })?;

        let rpc_hash = rpc_header.hash;
        let consensus = rpc_header.inner;
        let computed_hash = consensus.hash_slow();
        if rpc_hash != computed_hash {
            return Err(OutputRootError::HeaderHashMismatch {
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
        .map_err(|source| OutputRootError::AccountProofFailed { block_number, source })?;

        Ok(OutputRoot::from_parts(consensus.state_root, proof.storage_hash, computed_hash).hash())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, U256, address};
    use alloy_rpc_types_eth::Header as RpcHeader;

    use super::*;
    use crate::test_utils::{MockL2Provider, build_account_at_block, build_message_passer_proof};

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
            matches!(err, OutputRootError::BlockNotAvailable { block_number: 999, .. }),
            "expected BlockNotAvailable, got {err:?}"
        );
    }

    #[tokio::test]
    async fn compute_output_root_returns_block_not_available_when_header_missing() {
        let provider = MockL2Provider::new();
        let validator = L2OutputValidator::new(Arc::new(provider));

        let err = validator.compute_output_root(42).await.expect_err("must fail");

        assert!(
            matches!(err, OutputRootError::BlockNotAvailable { block_number: 42, .. }),
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
        provider
            .headers
            .insert(100, RpcHeader { hash: tampered_hash, inner: header, ..Default::default() });

        let validator = L2OutputValidator::new(Arc::new(provider));
        let err = validator.compute_output_root(100).await.expect_err("must fail");

        match err {
            OutputRootError::HeaderHashMismatch { block_number, rpc_hash, computed_hash: c } => {
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
            matches!(err, OutputRootError::AccountProofFailed { block_number: 100, .. }),
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

        assert!(matches!(err, OutputRootError::Rpc(_)), "expected Rpc error, got {err:?}");
    }
}
