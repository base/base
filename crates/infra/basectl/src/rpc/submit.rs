//! On-chain proposal-proof submission client for `basectl proofs submit`.

use std::{env, fmt, fs, path::Path, sync::Arc, time::Duration};

use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_proof_submission::{AggregateProofSubmitter, ProofSubmissionError, SnarkReceiptEncoder};
use base_prover_service_protocol::{GetProofResponse, ProofResult, ProofStatus};
use base_tx_manager::{
    NoopTxMetrics, SignerConfig, SimpleTxManager, TxManagerConfig, TxManagerError,
};
use tokio::time::timeout;
use url::Url;

use crate::errors::ProofsCommandError;

/// Upper bound for the initial L1 chain-ID request when connecting a
/// submitter, so a stalled endpoint cannot hang a one-shot command.
const CHAIN_ID_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound for the full `verifyProposalProof` send loop, fee bumps
/// included, so a post-publication RPC outage cannot hang the command.
const TX_SEND_TIMEOUT: Duration = Duration::from_secs(600);

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
    /// Environment variable holding the raw hex submitter private key.
    pub const PRIVATE_KEY_ENV: &'static str = "BASECTL_SUBMITTER_PRIVATE_KEY";

    /// Loads the submitter key from a key file or the environment.
    ///
    /// Prefers `key_file` when given; otherwise reads
    /// [`Self::PRIVATE_KEY_ENV`]. The key is deliberately never accepted as
    /// a command-line argument, so it cannot leak through shell history or
    /// the process list.
    pub fn load(key_file: Option<&Path>) -> Result<Self, ProofsCommandError> {
        if let Some(path) = key_file {
            let raw = fs::read_to_string(path).map_err(|source| {
                ProofsCommandError::ReadSubmitterKeyFile {
                    path: path.display().to_string(),
                    source,
                }
            })?;
            return Self::parse(&raw);
        }
        let raw =
            env::var(Self::PRIVATE_KEY_ENV).map_err(|_| ProofsCommandError::MissingSubmitterKey)?;
        Self::parse(&raw)
    }

    /// Parses a hex private key (with or without a `0x` prefix).
    pub fn parse(raw: &str) -> Result<Self, ProofsCommandError> {
        let signer = raw
            .trim()
            .parse::<PrivateKeySigner>()
            .map_err(|source| ProofsCommandError::InvalidSubmitterKey { source })?;
        Ok(Self { signer })
    }

    /// Returns the L1 wallet address controlled by this key.
    pub const fn address(&self) -> Address {
        self.signer.address()
    }
}

/// Decodes submittable PLONK proof bytes from a completed prover-service
/// session.
#[derive(Debug)]
pub struct SnarkPlonkProofBytes;

impl SnarkPlonkProofBytes {
    /// Extracts submittable PLONK proof bytes from a `getProof` response.
    ///
    /// Requires the session to have succeeded with a non-empty
    /// [`ProofResult::SnarkPlonk`] payload; every other shape maps to a
    /// specific [`ProofsCommandError`] explaining what to do next.
    pub fn from_response(
        session_id: &str,
        response: &GetProofResponse,
    ) -> Result<Bytes, ProofsCommandError> {
        match response.status {
            ProofStatus::Queued | ProofStatus::Running => {
                return Err(ProofsCommandError::ProofNotReady {
                    session_id: session_id.to_string(),
                    status: format!("{:?}", response.status).to_lowercase(),
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
            ProofResult::Compressed(_) | ProofResult::Tee(_) => {
                return Err(ProofsCommandError::NotAProposalProof {
                    session_id: session_id.to_string(),
                });
            }
        };
        SnarkReceiptEncoder::encode_onchain_zk_proof(&plonk.proof.proof).map_err(|error| {
            ProofsCommandError::InvalidProposalProof {
                session_id: session_id.to_string(),
                message: error.to_string(),
            }
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
pub struct ProposalProofSubmitter {
    tx_manager: SimpleTxManager<RootProvider>,
}

impl fmt::Debug for ProposalProofSubmitter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProposalProofSubmitter").finish_non_exhaustive()
    }
}

impl ProposalProofSubmitter {
    /// Connects to `l1_rpc` and builds a transaction manager for `key`.
    ///
    /// Fetches the chain ID from the endpoint and waits for one confirmation
    /// when sending, which is enough for a one-shot CLI submission. Both the
    /// chain-ID request and the overall send loop are time-bounded so a
    /// stalled RPC cannot hang a one-shot command indefinitely.
    pub async fn connect(l1_rpc: &Url, key: SubmitterKey) -> Result<Self, ProofsCommandError> {
        let provider = RootProvider::new_http(l1_rpc.clone());
        let chain_id = timeout(CHAIN_ID_TIMEOUT, provider.get_chain_id())
            .await
            .map_err(|_| ProofsCommandError::BuildTxManager {
                endpoint: l1_rpc.origin().ascii_serialization(),
                source: TxManagerError::Rpc(format!(
                    "fetching L1 chain ID timed out after {CHAIN_ID_TIMEOUT:?}"
                )),
            })?
            .map_err(|_| ProofsCommandError::BuildTxManager {
                // Origin only: operator L1 URLs commonly embed API keys in
                // the path or userinfo, which must not leak into error output.
                endpoint: l1_rpc.origin().ascii_serialization(),
                source: TxManagerError::Rpc("fetching L1 chain ID failed".to_string()),
            })?;
        let config = TxManagerConfig {
            num_confirmations: 1,
            tx_send_timeout: TX_SEND_TIMEOUT,
            ..TxManagerConfig::default()
        };
        let tx_manager = SimpleTxManager::new(
            provider,
            SignerConfig::local(key.signer),
            config,
            chain_id,
            Arc::new(NoopTxMetrics),
        )
        .await
        .map_err(|source| ProofsCommandError::BuildTxManager {
            endpoint: l1_rpc.origin().ascii_serialization(),
            source: Self::sanitize_tx_manager_error(source),
        })?;
        Ok(Self { tx_manager })
    }

    /// Submits `verifyProposalProof(proof)` to `game` and waits for the
    /// transaction to be mined.
    pub async fn submit(
        &self,
        game: Address,
        proof: Bytes,
    ) -> Result<SubmittedProof, ProofsCommandError> {
        let submitter = AggregateProofSubmitter::new(&self.tx_manager);
        let receipt = submitter.verify_proposal_proof(game, proof).await.map_err(|source| {
            let source = match source {
                ProofSubmissionError::TxManager(error) => {
                    ProofSubmissionError::TxManager(Self::sanitize_tx_manager_error(error))
                }
                other => other,
            };
            ProofsCommandError::Submission { game: game.to_string(), source }
        })?;
        Ok(SubmittedProof {
            tx_hash: receipt.transaction_hash,
            block_number: receipt.block_number,
            gas_used: receipt.gas_used,
        })
    }

    /// Replaces transport errors with a static message for command output.
    ///
    /// The transaction manager already sanitizes these errors before logging;
    /// this wrapper provides defense in depth for the returned error.
    fn sanitize_tx_manager_error(error: TxManagerError) -> TxManagerError {
        match error {
            TxManagerError::Rpc(_) => {
                TxManagerError::Rpc("L1 transport request failed".to_string())
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, address};
    use base_proof_submission::test_utils::SnarkReceiptFixture;
    use base_prover_service_protocol::{SnarkPlonkProofResult, ZkProofResult, ZkVm};

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
        Bytes::from(SnarkReceiptFixture::plonk_receipt_bytes([0x5a, 0x09, 0x3a, 0x2f], "abcd"))
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
    fn submitter_key_loads_from_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("submitter.key");
        std::fs::write(&path, format!("{TEST_KEY}\n")).expect("write key file");

        let key = SubmitterKey::load(Some(&path)).expect("key file loads");

        assert_eq!(key.address(), address!("7E5F4552091A69125d5DfCb7b8C2659029395Bdf"));
    }

    #[test]
    fn submitter_key_load_reports_unreadable_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("missing.key");

        let error = SubmitterKey::load(Some(&path)).expect_err("missing key file must fail");

        assert!(matches!(error, ProofsCommandError::ReadSubmitterKeyFile { .. }));
    }

    #[test]
    fn submitter_key_debug_hides_key_material() {
        let key = SubmitterKey::parse(TEST_KEY).expect("key parses");
        let debug = format!("{key:?}");
        assert!(debug.contains(&format!("{:?}", key.address())));
        assert!(!debug.to_lowercase().contains(&TEST_KEY[2..]));
    }

    #[test]
    fn submitter_sanitizes_l1_endpoint_secrets_from_transport_errors() {
        let endpoint =
            Url::parse("https://user:password@l1.example/v3/api-key?token=secret").unwrap();
        let error = TxManagerError::Rpc(format!("request to {endpoint} failed"));
        let sanitized = ProposalProofSubmitter::sanitize_tx_manager_error(error);
        let message = sanitized.to_string();

        assert!(message.contains("L1 transport request failed"));
        for secret in ["user", "password", "api-key", "token=secret"] {
            assert!(!message.contains(secret));
        }
    }

    #[test]
    fn from_response_extracts_plonk_bytes() {
        let response = succeeded_response(Some(plonk_result(encoded_plonk_receipt())));
        let extracted =
            SnarkPlonkProofBytes::from_response("session", &response).expect("proof extracts");
        assert_eq!(extracted.as_ref(), &[1, 0x5a, 0x09, 0x3a, 0x2f, 0xab, 0xcd]);
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
        assert!(matches!(error, ProofsCommandError::NotAProposalProof { .. }));
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
