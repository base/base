//! SP1 Network proving backend.
//!
//! Submits proofs to the SP1 prover network instead of a self-hosted cluster.
//! Enabled when `SP1_PROVER=network` is set. Uses the same witness generation
//! as the cluster backend but delegates proving to the decentralized SP1 network.

use std::{fmt, sync::Arc, time::Duration};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_proof_succinct_elfs::RANGE_ELF_EMBEDDED;
use base_proof_succinct_host_utils::get_agg_proof_stdin;
use base_zk_client::ProveBlockRequest;
use base_zk_db::{
    CreateProofSession, ProofRequest, ProofRequestRepo, ProofSession, ProofStatus, ProofType,
    SessionStatus as DbSessionStatus, SessionType, UpdateProofSession, UpdateReceipt,
};
use serde_json::json;
use sp1_sdk::{
    NetworkProver, ProveRequest, Prover, SP1ProofWithPublicValues,
    network::{FulfillmentStrategy, proto::types::FulfillmentStatus},
};
use tracing::{error, info, warn};

use super::{
    OpSuccinctProvider,
    traits::{
        BackendConfig, BackendType, ProofProcessingResult, ProveResult, ProvingBackend,
        SessionStatus,
    },
};

/// SP1 Network proving backend.
#[derive(Clone)]
pub struct NetworkBackend {
    provider: OpSuccinctProvider,
    config: BackendConfig,
}

impl fmt::Debug for NetworkBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetworkBackend").finish_non_exhaustive()
    }
}

impl NetworkBackend {
    /// Create a new backend with a pre-initialized provider and config.
    pub fn new(provider: OpSuccinctProvider, config: BackendConfig) -> Self {
        assert!(
            matches!(config, BackendConfig::Network { .. }),
            "NetworkBackend requires BackendConfig::Network"
        );
        Self { provider, config }
    }

    fn network_prover(&self) -> &Arc<NetworkProver> {
        let BackendConfig::Network { network_prover, .. } = &self.config else {
            unreachable!("validated in constructor")
        };
        network_prover
    }

    fn fulfillment_strategy(&self) -> FulfillmentStrategy {
        let BackendConfig::Network { fulfillment_strategy, .. } = &self.config else {
            unreachable!("validated in constructor")
        };
        *fulfillment_strategy
    }

    fn timeout(&self) -> Duration {
        let BackendConfig::Network { timeout_hours, .. } = &self.config else {
            unreachable!("validated in constructor")
        };
        Duration::from_secs(timeout_hours * 3600)
    }

    /// Parse a network proof ID from its hex string representation.
    fn parse_proof_id(proof_id_str: &str) -> anyhow::Result<B256> {
        proof_id_str
            .parse::<B256>()
            .map_err(|e| anyhow::anyhow!("invalid network proof ID '{proof_id_str}': {e}"))
    }
}

#[async_trait]
impl ProvingBackend for NetworkBackend {
    fn backend_type(&self) -> BackendType {
        BackendType::OpSuccinct
    }

    async fn prove(&self, request: &ProveBlockRequest) -> anyhow::Result<ProveResult> {
        let BackendConfig::Network {
            default_sequence_window,
            l1_node_url,
            base_consensus_url,
            network_prover,
            range_pk,
            timeout_hours,
            fulfillment_strategy,
            ..
        } = &self.config
        else {
            unreachable!("validated in constructor");
        };

        let start_block = request.start_block_number;
        let num_blocks = request.number_of_blocks_to_prove;
        let end_block = start_block + num_blocks;
        let sequence_window = request.sequence_window.unwrap_or(*default_sequence_window);

        let l1_head: Option<B256> = request
            .l1_head
            .as_ref()
            .map(|h| h.parse::<B256>())
            .transpose()
            .map_err(|e| anyhow::anyhow!("invalid l1_head hash: {e}"))?;

        info!(
            start_block = start_block,
            end_block = end_block,
            num_blocks = num_blocks,
            sequence_window = sequence_window,
            l1_head = ?l1_head,
            "starting SP1 Network proof generation"
        );

        // 1. Generate witness.
        let witness_start = std::time::Instant::now();
        let stdin = self
            .provider
            .generate_witness(
                start_block,
                end_block,
                sequence_window,
                l1_node_url,
                base_consensus_url,
                l1_head,
            )
            .await
            .map_err(|e| {
                error!(
                    start_block = start_block,
                    end_block = end_block,
                    error = %e,
                    "witness generation failed"
                );
                anyhow::anyhow!("witness generation failed: {e}")
            })?;
        let witness_gen_duration_ms = witness_start.elapsed().as_secs_f64() * 1000.0;

        info!(
            witness_gen_duration_ms = witness_gen_duration_ms,
            range_elf_bytes = RANGE_ELF_EMBEDDED.len(),
            "witness generated, submitting to SP1 Network"
        );

        // 2. Submit to SP1 Network.
        let timeout = Duration::from_secs(timeout_hours * 3600);
        let proof_id = network_prover
            .prove(range_pk, stdin)
            .compressed()
            .skip_simulation(true)
            .strategy(*fulfillment_strategy)
            .timeout(timeout)
            .request()
            .await
            .map_err(|e| {
                error!(error = %e, "failed to submit proof to SP1 Network");
                anyhow::anyhow!("failed to submit to SP1 Network: {e}")
            })?;

        info!(proof_id = %proof_id, "proof request submitted to SP1 Network");

        let metadata = json!({
            "backend": "sp1_network",
            "proof_id": proof_id.to_string(),
        });

        Ok(ProveResult {
            session_id: Some(proof_id.to_string()),
            metadata: Some(metadata),
            witness_gen_duration_ms: Some(witness_gen_duration_ms),
        })
    }

    async fn process_proof_request(
        &self,
        proof_request: &ProofRequest,
        repo: &ProofRequestRepo,
    ) -> anyhow::Result<ProofProcessingResult> {
        let sessions = repo.get_sessions_for_request(proof_request.id).await?;

        // Sync all RUNNING sessions with the SP1 Network.
        for session in &sessions {
            if session.status == DbSessionStatus::Running
                && let Err(e) =
                    self.sync_session_with_network(proof_request.id, session, repo).await
            {
                warn!(
                    proof_request_id = %proof_request.id,
                    session_id = %session.backend_session_id,
                    error = %e,
                    "failed to sync session with SP1 Network"
                );
            }
        }

        let updated_sessions = repo.get_sessions_for_request(proof_request.id).await?;

        // If SNARK requested, check if STARK completed and SNARK session needs triggering.
        if proof_request.proof_type == ProofType::OpSuccinctSp1ClusterSnarkGroth16 {
            let has_stark_completed = updated_sessions.iter().any(|s| {
                s.session_type == SessionType::Stark && s.status == DbSessionStatus::Completed
            });
            let has_snark_session =
                updated_sessions.iter().any(|s| s.session_type == SessionType::Snark);

            if has_stark_completed && !has_snark_session {
                info!(
                    proof_request_id = %proof_request.id,
                    "STARK completed, triggering stage-2 aggregation proof via SP1 Network"
                );
                let fresh_proof_request = repo.get(proof_request.id).await?.ok_or_else(|| {
                    anyhow::anyhow!("proof request not found after STARK completion")
                })?;
                if let Err(e) = self.submit_aggregation_proof(&fresh_proof_request, repo).await {
                    error!(
                        proof_request_id = %proof_request.id,
                        error = %e,
                        "failed to submit aggregation proof"
                    );
                    return Ok(ProofProcessingResult {
                        status: ProofStatus::Failed,
                        error_message: Some(format!("failed to submit aggregation proof: {e}")),
                    });
                }
                let updated_sessions = repo.get_sessions_for_request(proof_request.id).await?;
                return Ok(Self::determine_status(proof_request.proof_type, &updated_sessions));
            }
        }

        Ok(Self::determine_status(proof_request.proof_type, &updated_sessions))
    }

    async fn get_session_status(&self, session: &ProofSession) -> anyhow::Result<SessionStatus> {
        let proof_id = Self::parse_proof_id(&session.backend_session_id)?;
        let prover = self.network_prover();

        let (status, _proof) = prover
            .get_proof_status(proof_id)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get network proof status: {e}"))?;

        match FulfillmentStatus::try_from(status.fulfillment_status()) {
            Ok(FulfillmentStatus::Fulfilled) => Ok(SessionStatus::Completed),
            Ok(FulfillmentStatus::Unfulfillable) => {
                let reason =
                    format!("proof unfulfillable, execution_status={}", status.execution_status());
                Ok(SessionStatus::Failed(reason))
            }
            _ => Ok(SessionStatus::Running),
        }
    }

    fn name(&self) -> &'static str {
        "OP-Succinct (SP1 Network)"
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

impl NetworkBackend {
    async fn sync_session_with_network(
        &self,
        proof_request_id: uuid::Uuid,
        session: &ProofSession,
        repo: &ProofRequestRepo,
    ) -> anyhow::Result<()> {
        let proof_id = Self::parse_proof_id(&session.backend_session_id)?;
        let prover = self.network_prover();

        let (status, proof) = match prover.get_proof_status(proof_id).await {
            Ok(result) => result,
            Err(e) => {
                warn!(
                    proof_request_id = %proof_request_id,
                    session_id = %session.backend_session_id,
                    error = %e,
                    "transient error checking network proof status, will retry"
                );
                return Ok(());
            }
        };

        match FulfillmentStatus::try_from(status.fulfillment_status()) {
            Ok(FulfillmentStatus::Fulfilled) => {
                let proof_with_pv = proof.ok_or_else(|| {
                    anyhow::anyhow!("network status is Fulfilled but proof is None")
                })?;

                info!(
                    proof_request_id = %proof_request_id,
                    session_id = %session.backend_session_id,
                    "proof fulfilled by SP1 Network"
                );

                let proof_bytes =
                    bincode::serde::encode_to_vec(&proof_with_pv, bincode::config::standard())?;

                let update = UpdateProofSession {
                    backend_session_id: session.backend_session_id.clone(),
                    status: DbSessionStatus::Completed,
                    error_message: None,
                    metadata: session.metadata.clone(),
                };
                repo.update_proof_session(update).await?;

                let update_receipt = match session.session_type {
                    SessionType::Stark => UpdateReceipt {
                        id: proof_request_id,
                        stark_receipt: Some(proof_bytes),
                        snark_receipt: None,
                        status: ProofStatus::Running,
                        error_message: None,
                    },
                    SessionType::Snark => UpdateReceipt {
                        id: proof_request_id,
                        stark_receipt: None,
                        snark_receipt: Some(proof_bytes),
                        status: ProofStatus::Running,
                        error_message: None,
                    },
                };
                repo.update_receipt_if_running(update_receipt).await?;

                info!(
                    proof_request_id = %proof_request_id,
                    session_id = %session.backend_session_id,
                    "proof downloaded and stored"
                );
            }
            Ok(FulfillmentStatus::Unfulfillable) => {
                let reason =
                    format!("proof unfulfillable, execution_status={}", status.execution_status());
                error!(
                    proof_request_id = %proof_request_id,
                    session_id = %session.backend_session_id,
                    failure_detail = %reason,
                    "SP1 Network proof generation failed"
                );

                let update = UpdateProofSession {
                    backend_session_id: session.backend_session_id.clone(),
                    status: DbSessionStatus::Failed,
                    error_message: Some(reason),
                    metadata: session.metadata.clone(),
                };
                repo.update_proof_session(update).await?;
            }
            _ => {
                info!(
                    proof_request_id = %proof_request_id,
                    session_id = %session.backend_session_id,
                    "SP1 Network proof still running"
                );
            }
        }

        Ok(())
    }

    fn determine_status(proof_type: ProofType, sessions: &[ProofSession]) -> ProofProcessingResult {
        if sessions.is_empty() {
            return ProofProcessingResult { status: ProofStatus::Pending, error_message: None };
        }

        for session in sessions {
            if session.status == DbSessionStatus::Failed {
                return ProofProcessingResult {
                    status: ProofStatus::Failed,
                    error_message: session.error_message.clone(),
                };
            }
        }

        match proof_type {
            ProofType::OpSuccinctSp1ClusterCompressed => {
                let all_completed = sessions.iter().all(|s| s.status == DbSessionStatus::Completed);
                if all_completed {
                    ProofProcessingResult { status: ProofStatus::Succeeded, error_message: None }
                } else {
                    ProofProcessingResult { status: ProofStatus::Running, error_message: None }
                }
            }
            ProofType::OpSuccinctSp1ClusterSnarkGroth16 => {
                let stark_done = sessions.iter().any(|s| {
                    s.session_type == SessionType::Stark && s.status == DbSessionStatus::Completed
                });
                let snark_done = sessions.iter().any(|s| {
                    s.session_type == SessionType::Snark && s.status == DbSessionStatus::Completed
                });
                if stark_done && snark_done {
                    ProofProcessingResult { status: ProofStatus::Succeeded, error_message: None }
                } else {
                    ProofProcessingResult { status: ProofStatus::Running, error_message: None }
                }
            }
        }
    }

    async fn submit_aggregation_proof(
        &self,
        proof_request: &ProofRequest,
        repo: &ProofRequestRepo,
    ) -> anyhow::Result<()> {
        let BackendConfig::Network { network_prover, agg_pk, range_vk, .. } = &self.config else {
            unreachable!("validated in constructor");
        };

        let prover_address: alloy_primitives::Address = proof_request
            .prover_address
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("prover_address required for aggregation"))?
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid prover_address in DB: {e}"))?;

        // Deserialize STARK proof.
        let stark_receipt_bytes = proof_request
            .stark_receipt
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("STARK receipt not found for aggregation"))?;

        let (stark_proof_with_pv, _): (SP1ProofWithPublicValues, _) =
            bincode::serde::decode_from_slice(stark_receipt_bytes, bincode::config::standard())
                .map_err(|e| anyhow::anyhow!("failed to deserialize STARK proof: {e}"))?;

        // Extract boot_info from public values.
        let mut public_values = stark_proof_with_pv.public_values.clone();
        let boot_info: base_proof_succinct_client_utils::boot::BootInfoStruct =
            public_values.read();
        let boot_infos = vec![boot_info];
        let proofs = vec![stark_proof_with_pv.proof];

        // Fetch L1 headers for aggregation.
        let fetcher = self.provider.fetcher();
        let header = fetcher
            .get_latest_l1_head_in_batch(&boot_infos)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get latest L1 head in batch: {e}"))?;
        let l1_head_hash = header.hash_slow();

        let headers = fetcher
            .get_header_preimages(&boot_infos, l1_head_hash)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get header preimages: {e}"))?;

        info!(
            proof_request_id = %proof_request.id,
            l1_head_hash = %l1_head_hash,
            num_headers = headers.len(),
            "fetched L1 headers for aggregation proof"
        );

        // Build aggregation stdin.
        let stdin = get_agg_proof_stdin(
            proofs,
            boot_infos,
            headers,
            range_vk,
            l1_head_hash,
            prover_address,
        )
        .map_err(|e| anyhow::anyhow!("failed to build aggregation stdin: {e}"))?;

        // Submit aggregation ELF with Groth16 mode to SP1 Network.
        let timeout = self.timeout();
        let strategy = self.fulfillment_strategy();

        let proof_id = network_prover
            .prove(agg_pk, stdin)
            .mode(sp1_sdk::SP1ProofMode::Groth16)
            .strategy(strategy)
            .timeout(timeout)
            .request()
            .await
            .map_err(|e| {
                error!(error = %e, "failed to submit aggregation proof to SP1 Network");
                anyhow::anyhow!("failed to submit aggregation proof: {e}")
            })?;

        info!(
            proof_request_id = %proof_request.id,
            proof_id = %proof_id,
            "aggregation proof (Groth16) submitted to SP1 Network"
        );

        let metadata = json!({
            "backend": "sp1_network",
            "proof_id": proof_id.to_string(),
        });

        let session = CreateProofSession {
            proof_request_id: proof_request.id,
            session_type: SessionType::Snark,
            backend_session_id: proof_id.to_string(),
            metadata: Some(metadata),
        };

        repo.create_proof_session(session).await?;

        info!(
            proof_request_id = %proof_request.id,
            "created SNARK proof session for aggregation"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn make_session(session_type: SessionType, status: DbSessionStatus) -> ProofSession {
        let now = Utc::now();
        ProofSession {
            id: 1,
            proof_request_id: uuid::Uuid::new_v4(),
            session_type,
            backend_session_id: "test-session".to_string(),
            status,
            error_message: None,
            metadata: None,
            created_at: now,
            completed_at: None,
        }
    }

    fn make_failed_session(session_type: SessionType, error_msg: &str) -> ProofSession {
        let mut s = make_session(session_type, DbSessionStatus::Failed);
        s.error_message = Some(error_msg.to_string());
        s
    }

    #[test]
    fn test_determine_status_no_sessions_returns_pending() {
        let result =
            NetworkBackend::determine_status(ProofType::OpSuccinctSp1ClusterCompressed, &[]);
        assert_eq!(result.status, ProofStatus::Pending);
        assert!(result.error_message.is_none());
    }

    #[test]
    fn test_determine_status_compressed_all_completed() {
        let sessions = vec![make_session(SessionType::Stark, DbSessionStatus::Completed)];
        let result =
            NetworkBackend::determine_status(ProofType::OpSuccinctSp1ClusterCompressed, &sessions);
        assert_eq!(result.status, ProofStatus::Succeeded);
    }

    #[test]
    fn test_determine_status_compressed_running() {
        let sessions = vec![make_session(SessionType::Stark, DbSessionStatus::Running)];
        let result =
            NetworkBackend::determine_status(ProofType::OpSuccinctSp1ClusterCompressed, &sessions);
        assert_eq!(result.status, ProofStatus::Running);
    }

    #[test]
    fn test_determine_status_compressed_failed() {
        let sessions = vec![make_failed_session(SessionType::Stark, "network timeout")];
        let result =
            NetworkBackend::determine_status(ProofType::OpSuccinctSp1ClusterCompressed, &sessions);
        assert_eq!(result.status, ProofStatus::Failed);
        assert_eq!(result.error_message.as_deref(), Some("network timeout"));
    }

    #[test]
    fn test_determine_status_snark_both_completed() {
        let sessions = vec![
            make_session(SessionType::Stark, DbSessionStatus::Completed),
            make_session(SessionType::Snark, DbSessionStatus::Completed),
        ];
        let result = NetworkBackend::determine_status(
            ProofType::OpSuccinctSp1ClusterSnarkGroth16,
            &sessions,
        );
        assert_eq!(result.status, ProofStatus::Succeeded);
    }

    #[test]
    fn test_determine_status_snark_only_stark_completed() {
        let sessions = vec![make_session(SessionType::Stark, DbSessionStatus::Completed)];
        let result = NetworkBackend::determine_status(
            ProofType::OpSuccinctSp1ClusterSnarkGroth16,
            &sessions,
        );
        assert_eq!(result.status, ProofStatus::Running);
    }

    #[test]
    fn test_determine_status_failure_takes_priority() {
        let sessions = vec![
            make_session(SessionType::Stark, DbSessionStatus::Completed),
            make_failed_session(SessionType::Snark, "failure wins"),
        ];
        let result =
            NetworkBackend::determine_status(ProofType::OpSuccinctSp1ClusterCompressed, &sessions);
        assert_eq!(result.status, ProofStatus::Failed);
    }

    #[test]
    fn test_parse_proof_id_valid() {
        let hex = "0x0000000000000000000000000000000000000000000000000000000000000001";
        let result = NetworkBackend::parse_proof_id(hex);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            B256::from_slice(&[0; 31].into_iter().chain([1]).collect::<Vec<u8>>())
        );
    }

    #[test]
    fn test_parse_proof_id_invalid() {
        let result = NetworkBackend::parse_proof_id("not-a-hash");
        assert!(result.is_err());
    }
}
