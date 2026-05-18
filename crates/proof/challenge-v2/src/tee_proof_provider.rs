//! TEE proving service abstraction.
//!
//! [`TeeProofProvider`] is the trait the challenger uses to ask its TEE
//! prover to attest a range of L2 blocks. The fast path for `TeeWrong`
//! disputes (see `crate::prove`) calls this trait first; the result
//! (signed root + signature bytes) is wrapped into a TEE-flavored
//! dispute action when it matches our computed view.
//!
//! [`RpcTeeProofProvider`] is the production implementation backed by
//! an off-chain TEE service exposed via JSON-RPC. It translates the
//! trait's shape into the full [`ProofRequest`] the service expects,
//! looking up the missing `l1_head_number` and `agreed_l2_head_hash`
//! from the L1 and L2 providers respectively.

use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes};
use async_trait::async_trait;
use base_proof_primitives::{ProofRequest, ProofResult, ProverClient};
use base_proof_rpc::{L1Provider, L2Provider};
use derive_more::Debug;
use thiserror::Error;

/// Output of a TEE proving call: the root the TEE attested to plus
/// the raw 65-byte ECDSA signature over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeeProofResult {
    /// Output root the TEE attested to.
    pub signed_root: B256,
    /// Raw 65-byte ECDSA signature over `signed_root` (`r || s || v`).
    /// The encoder normalizes `v` to 27/28 before submission.
    pub signature_bytes: Bytes,
}

/// Errors returned by [`TeeProofProvider::prove_range`].
#[derive(Debug, Error)]
pub enum TeeProofError {
    /// TEE backend produced an error (transport, signing, attestation, ...).
    #[error("TEE backend error: {0}")]
    Backend(String),
}

/// TEE proving service abstraction.
///
/// Implementations sign an attestation over a range of L2 blocks
/// and return the signed root plus signature bytes. Cheap (seconds,
/// not hours) compared to ZK proving.
#[async_trait]
pub trait TeeProofProvider: Send + Sync + std::fmt::Debug {
    /// Asks the TEE to sign an attestation over the L2 block range
    /// `[start_block, end_block]` rooted at `l1_head`, sampling
    /// intermediate roots at the given checkpoint interval.
    async fn prove_range(
        &self,
        start_block: u64,
        start_root: B256,
        end_block: u64,
        end_root: B256,
        l1_head: B256,
        intermediate_block_interval: u64,
    ) -> Result<TeeProofResult, TeeProofError>;
}

/// Production [`TeeProofProvider`] backed by an RPC TEE service.
#[derive(Debug)]
pub struct RpcTeeProofProvider {
    /// JSON-RPC client over the TEE service.
    #[debug(skip)]
    prover: Arc<dyn ProverClient>,
    /// L1 reader used to resolve `l1_head_number` from a head hash.
    #[debug(skip)]
    l1: Arc<dyn L1Provider>,
    /// L2 reader used to resolve `agreed_l2_head_hash` from a block number.
    #[debug(skip)]
    l2: Arc<dyn L2Provider>,
    /// Address that will submit the dispute transaction on L1; committed
    /// in the TEE journal so the on-chain verifier can match `msg.sender`.
    proposer: Address,
}

impl RpcTeeProofProvider {
    /// Builds a provider over the given prover client and chain readers.
    pub const fn new(
        prover: Arc<dyn ProverClient>,
        l1: Arc<dyn L1Provider>,
        l2: Arc<dyn L2Provider>,
        proposer: Address,
    ) -> Self {
        Self { prover, l1, l2, proposer }
    }
}

#[async_trait]
impl TeeProofProvider for RpcTeeProofProvider {
    async fn prove_range(
        &self,
        start_block: u64,
        start_root: B256,
        end_block: u64,
        end_root: B256,
        l1_head: B256,
        intermediate_block_interval: u64,
    ) -> Result<TeeProofResult, TeeProofError> {
        let (l1_header, l2_header) = tokio::try_join!(
            self.l1.header_by_hash(l1_head),
            self.l2.header_by_number(Some(start_block)),
        )
        .map_err(|e| TeeProofError::Backend(format!("chain lookup failed: {e}")))?;

        let request = ProofRequest {
            l1_head,
            agreed_l2_head_hash: l2_header.hash,
            agreed_l2_output_root: start_root,
            claimed_l2_output_root: end_root,
            claimed_l2_block_number: end_block,
            proposer: self.proposer,
            intermediate_block_interval,
            l1_head_number: l1_header.number,
            // Field reserved in the protocol struct; no server path in
            // this repo reads it.
            image_hash: B256::ZERO,
        };

        let result =
            self.prover.prove(request).await.map_err(|e| TeeProofError::Backend(e.to_string()))?;

        match result {
            ProofResult::Tee { aggregate_proposal, .. } => Ok(TeeProofResult {
                signed_root: aggregate_proposal.output_root,
                signature_bytes: aggregate_proposal.signature,
            }),
            ProofResult::Zk { .. } => {
                Err(TeeProofError::Backend("prover returned a ZK result for a TEE request".into()))
            }
        }
    }
}
