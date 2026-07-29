//! On-chain proposal-proof submission client for `basectl proofs submit`.

use std::{fmt, sync::Arc};

use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_proof_primitives::ProofEncoder;
use base_proof_submission::AggregateProofSubmitter;
use base_prover_service_protocol::{GetProofResponse, ProofResult, ProofStatus};
use base_tx_manager::{NoopTxMetrics, SignerConfig, SimpleTxManager, TxManager, TxManagerConfig};
use sp1_sdk::SP1ProofWithPublicValues;
use url::Url;

use crate::errors::ProofsCommandError;

/// Parsed L1 submitter private key for `basectl proofs submit`.
///
/// The wallet behind this key must be the exact `--prover-address` the proof
/// was proposed with, because the proof journal commits to that address as
/// the proposer.
#[derive(Clone)]
pub struct SubmitterKey {
    signer: PrivateKeySigner,
}

impl fmt::Debug for SubmitterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubmitterKey").field("address", &self.signer.address()).finish()
    }
}

impl SubmitterKey {
    /// Parses a hex private key (with or without a `0x` prefix).
    pub fn parse(raw: &str) -> Result<Self, ProofsCommandError> {
        let signer = raw.trim().parse::<PrivateKeySigner>().map_err(|error| {
            ProofsCommandError::InvalidSubmitterKey { message: error.to_string() }
        })?;
        Ok(Self { signer })
    }

    /// Returns the L1 wallet address controlled by this key.
    pub const fn address(&self) -> Address {
        self.signer.address()
    }
}

/// PLONK proof bytes extracted from a completed prover-service session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnarkPlonkProofBytes {
    /// The wrapped PLONK proof bytes to submit on chain.
    pub proof: Bytes,
}

impl SnarkPlonkProofBytes {
    /// Extracts submittable PLONK proof bytes from a `getProof` response.
    ///
    /// Requires the session to have succeeded with a non-empty
    /// [`ProofResult::SnarkPlonk`] payload; every other shape maps to a
    /// specific [`ProofsCommandError`] explaining what to do next.
    pub fn from_response(
        session_id: &str,
        response: &GetProofResponse,
    ) -> Result<Self, ProofsCommandError> {
        match response.status {
            ProofStatus::Queued => {
                return Err(ProofsCommandError::ProofNotReady {
                    session_id: session_id.to_string(),
                    status: "queued".to_string(),
                });
            }
            ProofStatus::Running => {
                return Err(ProofsCommandError::ProofNotReady {
                    session_id: session_id.to_string(),
                    status: "running".to_string(),
                });
            }
            ProofStatus::Failed => {
                return Err(ProofsCommandError::ProofFailed {
                    session_id: session_id.to_string(),
                    message: response
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "unknown error".to_string()),
                });
            }
            ProofStatus::Succeeded => {}
        }

        let result = response.result.as_ref().ok_or_else(|| {
            ProofsCommandError::ProofResultMissing { session_id: session_id.to_string() }
        })?;
        let plonk = match result {
            ProofResult::SnarkPlonk(plonk) => plonk,
            ProofResult::Compressed(_) => {
                return Err(ProofsCommandError::NotAProposalProof {
                    session_id: session_id.to_string(),
                    actual: "compressed",
                });
            }
            ProofResult::Tee(_) => {
                return Err(ProofsCommandError::NotAProposalProof {
                    session_id: session_id.to_string(),
                    actual: "tee",
                });
            }
        };
        if plonk.proof.proof.is_empty() {
            return Err(ProofsCommandError::EmptyProofBytes { session_id: session_id.to_string() });
        }
        let (receipt, _): (SP1ProofWithPublicValues, _) =
            bincode::serde::decode_from_slice(&plonk.proof.proof, bincode::config::standard())
                .map_err(|error| ProofsCommandError::InvalidProposalProof {
                    session_id: session_id.to_string(),
                    message: error.to_string(),
                })?;
        Ok(Self {
            proof: ProofEncoder::encode_zk_dispute_proof_bytes(Bytes::from(receipt.bytes())),
        })
    }
}

/// Receipt data for a mined `verifyProposalProof` transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmittedProof {
    /// The L1 transaction hash.
    pub tx_hash: B256,
    /// The L1 block the transaction was included in.
    pub block_number: Option<u64>,
    /// Gas used by the transaction.
    pub gas_used: u64,
}

/// Sends `AggregateVerifier.verifyProposalProof` transactions to L1 dispute
/// games, signed by a [`SubmitterKey`].
#[derive(Debug)]
pub struct ProposalProofSubmitter {
    tx_manager: SimpleTxManager<RootProvider>,
}

impl ProposalProofSubmitter {
    /// Connects to `l1_rpc` and builds a transaction manager for `key`.
    ///
    /// Fetches the chain ID from the endpoint and waits for one confirmation
    /// when sending, which is enough for a one-shot CLI submission.
    pub async fn connect(l1_rpc: &Url, key: SubmitterKey) -> Result<Self, ProofsCommandError> {
        let build_error = |message: String| ProofsCommandError::BuildTxManager {
            endpoint: l1_rpc.to_string(),
            message,
        };
        let provider = RootProvider::new_http(l1_rpc.clone());
        let chain_id = provider
            .get_chain_id()
            .await
            .map_err(|error| build_error(format!("fetching L1 chain ID: {error}")))?;
        let config = TxManagerConfig { num_confirmations: 1, ..TxManagerConfig::default() };
        let tx_manager = SimpleTxManager::new(
            provider,
            SignerConfig::local(key.signer),
            config,
            chain_id,
            Arc::new(NoopTxMetrics),
        )
        .await
        .map_err(|error| build_error(error.to_string()))?;
        Ok(Self { tx_manager })
    }

    /// Returns the L1 wallet address that signs submissions.
    pub fn sender_address(&self) -> Address {
        self.tx_manager.sender_address()
    }

    /// Submits `verifyProposalProof(proof)` to `game` and waits for the
    /// transaction to be mined.
    pub async fn submit(
        &self,
        game: Address,
        proof: Bytes,
    ) -> Result<SubmittedProof, ProofsCommandError> {
        let submitter = AggregateProofSubmitter::new(&self.tx_manager);
        let receipt = submitter
            .verify_proposal_proof(game, proof)
            .await
            .map_err(|source| ProofsCommandError::Submission { game: game.to_string(), source })?;
        Ok(SubmittedProof {
            tx_hash: receipt.transaction_hash,
            block_number: receipt.block_number,
            gas_used: receipt.gas_used,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, address};
    use base_prover_service_protocol::{SnarkPlonkProofResult, ZkProofResult, ZkVm};
    use sp1_sdk::{SP1Proof, SP1ProofWithPublicValues, SP1PublicValues};

    use super::*;

    const TEST_KEY: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";

    fn succeeded_response(result: Option<ProofResult>) -> GetProofResponse {
        GetProofResponse { status: ProofStatus::Succeeded, error_message: None, result }
    }

    fn plonk_result(proof: Bytes) -> ProofResult {
        ProofResult::SnarkPlonk(SnarkPlonkProofResult {
            proof: ZkProofResult { zk_vm: ZkVm::Sp1, proof, execution_stats: None },
        })
    }

    fn encoded_plonk_receipt() -> Bytes {
        let mut plonk_vkey_hash = [0u8; 32];
        plonk_vkey_hash[..4].copy_from_slice(&[0x5a, 0x09, 0x3a, 0x2f]);
        let mut receipt = SP1ProofWithPublicValues {
            proof: SP1Proof::Plonk(Default::default()),
            public_values: SP1PublicValues::new(),
            sp1_version: "test".to_owned(),
            tee_proof: None,
        };
        let SP1Proof::Plonk(plonk) = &mut receipt.proof else {
            unreachable!();
        };
        plonk.encoded_proof = "abcd".to_owned();
        plonk.plonk_vkey_hash = plonk_vkey_hash;

        Bytes::from(bincode::serde::encode_to_vec(&receipt, bincode::config::standard()).unwrap())
    }

    #[test]
    fn submitter_key_parses_with_and_without_prefix() {
        let with_prefix = SubmitterKey::parse(TEST_KEY).expect("0x-prefixed key parses");
        let without_prefix =
            SubmitterKey::parse(TEST_KEY.trim_start_matches("0x")).expect("bare key parses");
        assert_eq!(with_prefix.address(), without_prefix.address());
        assert_eq!(with_prefix.address(), address!("7E5F4552091A69125d5DfCb7b8C2659029395Bdf"),);
    }

    #[test]
    fn submitter_key_rejects_invalid_input() {
        let error = SubmitterKey::parse("not-a-key").expect_err("garbage key must fail");
        assert!(matches!(error, ProofsCommandError::InvalidSubmitterKey { .. }));
    }

    #[test]
    fn submitter_key_debug_hides_key_material() {
        let key = SubmitterKey::parse(TEST_KEY).expect("key parses");
        let debug = format!("{key:?}");
        assert!(debug.contains(&format!("{:?}", key.address())));
        assert!(!debug.to_lowercase().contains(&TEST_KEY[2..]));
    }

    #[test]
    fn from_response_extracts_plonk_bytes() {
        let response = succeeded_response(Some(plonk_result(encoded_plonk_receipt())));
        let extracted =
            SnarkPlonkProofBytes::from_response("session", &response).expect("proof extracts");
        assert_eq!(extracted.proof.as_ref(), &[1, 0x5a, 0x09, 0x3a, 0x2f, 0xab, 0xcd]);
    }

    #[test]
    fn from_response_rejects_incomplete_sessions() {
        for (status, label) in [(ProofStatus::Queued, "queued"), (ProofStatus::Running, "running")]
        {
            let response = GetProofResponse { status, error_message: None, result: None };
            let error = SnarkPlonkProofBytes::from_response("session", &response)
                .expect_err("incomplete session must fail");
            match error {
                ProofsCommandError::ProofNotReady { status, .. } => assert_eq!(status, label),
                other => panic!("expected ProofNotReady, got {other:?}"),
            }
        }
    }

    #[test]
    fn from_response_rejects_failed_sessions() {
        let response = GetProofResponse {
            status: ProofStatus::Failed,
            error_message: Some("witness generation failed".to_string()),
            result: None,
        };
        let error = SnarkPlonkProofBytes::from_response("session", &response)
            .expect_err("failed session must fail");
        match error {
            ProofsCommandError::ProofFailed { message, .. } => {
                assert_eq!(message, "witness generation failed");
            }
            other => panic!("expected ProofFailed, got {other:?}"),
        }
    }

    #[test]
    fn from_response_rejects_missing_result() {
        let response = succeeded_response(None);
        let error = SnarkPlonkProofBytes::from_response("session", &response)
            .expect_err("missing result must fail");
        assert!(matches!(error, ProofsCommandError::ProofResultMissing { .. }));
    }

    #[test]
    fn from_response_rejects_wrong_proof_types() {
        let compressed = ProofResult::Compressed(ZkProofResult {
            zk_vm: ZkVm::Sp1,
            proof: Bytes::from_static(b"compressed"),
            execution_stats: None,
        });
        let error =
            SnarkPlonkProofBytes::from_response("session", &succeeded_response(Some(compressed)))
                .expect_err("compressed result must fail");
        assert!(matches!(
            error,
            ProofsCommandError::NotAProposalProof { actual: "compressed", .. }
        ));
    }

    #[test]
    fn from_response_rejects_empty_proof_bytes() {
        let response = succeeded_response(Some(plonk_result(Bytes::new())));
        let error = SnarkPlonkProofBytes::from_response("session", &response)
            .expect_err("empty proof bytes must fail");
        assert!(matches!(error, ProofsCommandError::EmptyProofBytes { .. }));
    }

    #[test]
    fn from_response_rejects_invalid_plonk_receipt() {
        let response =
            succeeded_response(Some(plonk_result(Bytes::from_static(b"not-an-sp1-receipt"))));
        let error = SnarkPlonkProofBytes::from_response("session", &response)
            .expect_err("invalid receipt must fail");
        assert!(matches!(error, ProofsCommandError::InvalidProposalProof { .. }));
    }
}
