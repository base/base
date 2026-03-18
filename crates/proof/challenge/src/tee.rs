//! TEE proof provider abstraction and encoding for TEE-first proof sourcing.

use alloy_primitives::Bytes;
use async_trait::async_trait;
use base_enclave::ProofEncoder;
use base_enclave_client::EnclaveClient;
use base_proof_primitives::{ProofRequest, ProofResult};

/// Trait for sourcing proofs from a TEE enclave.
#[async_trait]
pub trait TeeProofProvider: Send + Sync + std::fmt::Debug {
    /// Sends a proof request to the TEE backend and returns the result.
    async fn prove(&self, request: ProofRequest) -> eyre::Result<ProofResult>;
}

/// [`TeeProofProvider`] backed by an [`EnclaveClient`] RPC connection.
#[derive(Debug)]
pub struct EnclaveTeeProvider {
    client: EnclaveClient,
}

impl EnclaveTeeProvider {
    /// Creates a new provider wrapping the given enclave client.
    pub const fn new(client: EnclaveClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl TeeProofProvider for EnclaveTeeProvider {
    async fn prove(&self, request: ProofRequest) -> eyre::Result<ProofResult> {
        self.client.prove(request).await.map_err(Into::into)
    }
}

/// Encodes a [`ProofResult::Tee`] into the 130-byte format expected by the
/// `AggregateVerifier` contract.
///
/// Format: `proofType(1) + l1OriginHash(32) + l1OriginNumber(32) + signature(65)`
pub fn encode_tee_proof(result: &ProofResult) -> eyre::Result<Bytes> {
    match result {
        ProofResult::Tee { aggregate_proposal, .. } => {
            let bytes = ProofEncoder::encode_proof_bytes(
                &aggregate_proposal.signature,
                aggregate_proposal.l1_origin_hash,
                aggregate_proposal.l1_origin_number,
            )
            .map_err(|e| eyre::eyre!("TEE proof encoding failed: {e}"))?;
            Ok(bytes)
        }
        ProofResult::Zk { .. } => {
            Err(eyre::eyre!("expected ProofResult::Tee, got ProofResult::Zk"))
        }
    }
}
