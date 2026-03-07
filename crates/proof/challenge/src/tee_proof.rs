//! TEE-based nullification proof generation for the challenger.
//!
//! Delegates proof orchestration (per-block execution, aggregation) to the TEE
//! server via [`EnclaveProvider::prove`] and encodes the resulting aggregate
//! proposal into the on-chain proof format.
//!
//! ## Proof format
//!
//! The 130-byte output matches the `AggregateVerifier` contract interface:
//!
//! | Offset | Length | Field |
//! |--------|--------|-------|
//! | 0      | 1      | Proof type (`0x00` = TEE) |
//! | 1      | 32     | L1 origin block hash |
//! | 33     | 32     | L1 origin block number (big-endian `uint256`) |
//! | 65     | 65     | ECDSA signature (v-value adjusted to 27/28) |
//!
//! ## Availability
//!
//! TEE proof generation is gated behind the optional `--tee-endpoint` CLI
//! flag (`CHALLENGER_TEE_ENDPOINT`). When the endpoint is not configured,
//! the orchestrator skips TEE and falls back to ZK proof generation.

use std::sync::Arc;

use alloy_primitives::Bytes;
use base_enclave_client::{ClientError, EnclaveProvider, ProofRequest};
use base_tee_prover::ProofEncoder;
use thiserror::Error;
use tracing::info;

/// Errors that can occur during TEE proof generation.
#[derive(Debug, Error)]
pub enum TeeProofError {
    /// The TEE enclave returned an error.
    #[error(transparent)]
    Enclave(#[from] ClientError),

    /// Non-RPC data preparation failed (arithmetic overflows, missing data, derivation errors).
    #[error("data preparation failed: {0}")]
    DataPrep(String),

    /// Proof encoding or data transformation failed.
    #[error("proof encoding failed: {0}")]
    Encoding(String),
}

impl TeeProofError {
    /// Returns `true` if the error is transient and the operation can be retried.
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Enclave(e) => e.is_retryable(),
            Self::DataPrep(_) | Self::Encoding(_) => false,
        }
    }
}

/// Generates TEE-based nullification proofs for invalid candidate games.
///
/// Delegates all proof orchestration (per-block execution and aggregation) to
/// the TEE server via [`EnclaveProvider::prove`], then encodes the aggregate
/// proposal into the 130-byte on-chain format.
#[derive(Debug)]
pub struct TeeProofGenerator<P> {
    /// TEE prover that handles proof generation.
    prover: Arc<P>,
}

impl<P: EnclaveProvider> TeeProofGenerator<P> {
    /// Creates a new TEE proof generator.
    #[must_use]
    pub const fn new(prover: Arc<P>) -> Self {
        Self { prover }
    }

    /// Generates a TEE nullification proof for a [`ProofRequest`].
    ///
    /// The caller is responsible for constructing the request with the correct
    /// `l1_head`, `agreed_l2_head_hash`, `agreed_l2_output_root`,
    /// `claimed_l2_output_root`, and `claimed_l2_block_number`.
    ///
    /// # Returns
    ///
    /// 130-byte proof bytes: `proofType(1) + l1OriginHash(32) + l1OriginNumber(32) + signature(65)`
    ///
    /// # Errors
    ///
    /// Returns [`TeeProofError`] if the enclave call or proof encoding fails.
    pub async fn generate_tee_proof(&self, request: ProofRequest) -> Result<Bytes, TeeProofError> {
        info!(
            claimed_block = %request.claimed_l2_block_number,
            "generating TEE proof"
        );

        let result = self.prover.prove(request).await?;

        let proposal = &result.claim.aggregate_proposal;
        let l1_origin_number: u64 = proposal
            .l1_origin_number
            .try_into()
            .map_err(|_| TeeProofError::DataPrep("L1 origin number overflows u64".into()))?;

        let proof_bytes = ProofEncoder::encode_proof_bytes(
            &proposal.signature,
            proposal.l1_origin_hash,
            l1_origin_number,
        )
        .map_err(|e| TeeProofError::Encoding(e.to_string()))?;

        info!(
            proof_len = %proof_bytes.len(),
            "TEE proof generated"
        );

        Ok(proof_bytes)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, U256};
    use async_trait::async_trait;
    use base_enclave_client::{ClientError, ProofClaim, ProofEvidence, ProofRequest, ProofResult};
    use base_proof_primitives::Proposal;
    use base_tee_prover::PROOF_TYPE_TEE;

    use super::*;

    // ========================================================================
    // Mock
    // ========================================================================

    #[derive(Debug)]
    struct MockTeeProver {
        result: Result<ProofResult, ClientError>,
    }

    impl MockTeeProver {
        fn ok(proposal: Proposal) -> Self {
            Self {
                result: Ok(ProofResult {
                    claim: ProofClaim {
                        aggregate_proposal: proposal.clone(),
                        proposals: vec![proposal],
                    },
                    evidence: ProofEvidence::Tee { attestation_doc: vec![], signature: vec![] },
                }),
            }
        }

        fn err(error: ClientError) -> Self {
            Self { result: Err(error) }
        }
    }

    #[async_trait]
    impl EnclaveProvider for MockTeeProver {
        async fn prove(&self, _request: ProofRequest) -> Result<ProofResult, ClientError> {
            match &self.result {
                Ok(r) => Ok(r.clone()),
                Err(e) => Err(ClientError::ClientCreation(e.to_string())),
            }
        }
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    fn test_proposal(l1_hash: B256, l1_number: u64) -> Proposal {
        let mut sig = vec![0xAB; 65];
        sig[64] = 0;
        Proposal {
            output_root: B256::repeat_byte(0x11),
            signature: Bytes::from(sig),
            l1_origin_hash: l1_hash,
            l1_origin_number: U256::from(l1_number),
            l2_block_number: U256::from(100u64),
            prev_output_root: B256::ZERO,
            config_hash: B256::ZERO,
        }
    }

    fn test_request() -> ProofRequest {
        ProofRequest {
            l1_head: B256::repeat_byte(0x01),
            agreed_l2_head_hash: B256::repeat_byte(0x02),
            agreed_l2_output_root: B256::repeat_byte(0x03),
            claimed_l2_output_root: B256::repeat_byte(0x04),
            claimed_l2_block_number: 110,
        }
    }

    // ========================================================================
    // Happy path
    // ========================================================================

    #[tokio::test]
    async fn test_generate_tee_proof_happy_path() {
        let l1_origin_hash = B256::repeat_byte(0xEE);
        let prover = MockTeeProver::ok(test_proposal(l1_origin_hash, 500));
        let generator = TeeProofGenerator::new(Arc::new(prover));

        let result = generator.generate_tee_proof(test_request()).await;
        assert!(result.is_ok(), "expected Ok, got: {}", result.unwrap_err());

        let proof = result.unwrap();

        // 130-byte proof format
        assert_eq!(proof.len(), 130);
        assert_eq!(proof[0], PROOF_TYPE_TEE);
        assert_eq!(&proof[1..33], l1_origin_hash.as_slice());
        // L1 origin number as big-endian uint256 (24 zero bytes + 8 bytes)
        assert_eq!(&proof[33..57], &[0u8; 24]);
        let mut number_bytes = [0u8; 8];
        number_bytes.copy_from_slice(&proof[57..65]);
        assert_eq!(u64::from_be_bytes(number_bytes), 500);
        // v-value adjusted to 27
        assert_eq!(proof[129], 27);
    }

    // ========================================================================
    // Enclave error
    // ========================================================================

    #[tokio::test]
    async fn test_generate_tee_proof_enclave_error() {
        let prover = MockTeeProver::err(ClientError::ClientCreation("enclave down".into()));
        let generator = TeeProofGenerator::new(Arc::new(prover));

        let result = generator.generate_tee_proof(test_request()).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::Enclave(_)), "expected Enclave error, got: {err}");
    }

    // ========================================================================
    // Error retryability
    // ========================================================================

    #[test]
    fn test_enclave_error_is_retryable_when_client_error_is() {
        let err = TeeProofError::Enclave(ClientError::Rpc(
            jsonrpsee::core::client::Error::RequestTimeout,
        ));
        assert!(err.is_retryable());
    }

    #[test]
    fn test_enclave_error_not_retryable_for_creation_failure() {
        let err = TeeProofError::Enclave(ClientError::ClientCreation("bad url".into()));
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_data_prep_not_retryable() {
        let err = TeeProofError::DataPrep("overflow".into());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_encoding_not_retryable() {
        let err = TeeProofError::Encoding("bad bytes".into());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_error_display() {
        let err = TeeProofError::DataPrep("arithmetic overflow".into());
        assert_eq!(err.to_string(), "data preparation failed: arithmetic overflow");

        let err = TeeProofError::Encoding("bad bytes".into());
        assert_eq!(err.to_string(), "proof encoding failed: bad bytes");
    }
}
