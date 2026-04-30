//! OP-Succinct proving backend using the SP1 cluster.

use std::{fmt, time::SystemTime};

use async_trait::async_trait;
use base_proof_succinct_elfs::{AGGREGATION_ELF, RANGE_ELF_EMBEDDED};
use base_proof_succinct_host_utils::get_agg_proof_stdin;
use base_zk_client::ProveBlockRequest;
use base_zk_db::{
    CreateProofSession, ProofRequest, ProofRequestRepo, ProofSession, ProofStatus, ProofType,
    SessionStatus as DbSessionStatus, SessionType, UpdateProofSession, UpdateReceipt,
};
use serde_json::json;
use sp1_cluster_common::proto::{
    ExecutionFailureCause, ExecutionStatus, ProofRequest as ClusterProtoProofRequest,
    ProofRequestStatus,
};
use sp1_cluster_utils::{ClusterElf, ProofRequestConfig, create_request};
use sp1_prover_types::{Artifact, ArtifactClient as _, ArtifactType};
use sp1_sdk::{ProofFromNetwork, SP1ProofWithPublicValues, network::proto::types::ProofMode};
use tracing::{error, info, warn};
use uuid::Uuid;

use super::provider::OpSuccinctProvider;
use crate::backends::traits::{
    ArtifactClientWrapper, BackendConfig, BackendType, ProofProcessingResult, ProveResult,
    ProvingBackend, SessionStatus,
};

/// OP Succinct proving backend.
#[derive(Clone)]
pub struct OpSuccinctBackend {
    provider: OpSuccinctProvider,
    config: BackendConfig,
}

impl fmt::Debug for OpSuccinctBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpSuccinctBackend").finish_non_exhaustive()
    }
}

impl OpSuccinctBackend {
    /// Create a new backend with a pre-initialized provider and config.
    pub fn new(provider: OpSuccinctProvider, config: BackendConfig) -> Self {
        assert!(
            matches!(config, BackendConfig::OpSuccinct { .. }),
            "OpSuccinctBackend requires BackendConfig::OpSuccinct"
        );
        Self { provider, config }
    }
}

#[async_trait]
impl ProvingBackend for OpSuccinctBackend {
    fn backend_type(&self) -> BackendType {
        BackendType::OpSuccinct
    }

    async fn prove(&self, request: &ProveBlockRequest) -> anyhow::Result<ProveResult> {
        let BackendConfig::OpSuccinct {
            default_sequence_window,
            cluster_rpc,
            artifact_client,
            artifact_storage_config,
            timeout_hours,
            l1_node_url,
            base_consensus_url,
            cluster_client,
            ..
        } = &self.config
        else {
            unreachable!("validated in constructor");
        };

        let start_block = request.start_block_number;
        let num_blocks = request.number_of_blocks_to_prove;
        let end_block = start_block + num_blocks;
        let sequence_window = request.sequence_window.unwrap_or(*default_sequence_window);

        let l1_head: Option<alloy_primitives::B256> = request
            .l1_head
            .as_ref()
            .map(|h| h.parse::<alloy_primitives::B256>())
            .transpose()
            .map_err(|e| anyhow::anyhow!("Invalid l1_head hash: {e}"))?;

        info!(
            start_block = start_block,
            end_block = end_block,
            num_blocks = num_blocks,
            sequence_window = sequence_window,
            l1_head = ?l1_head,
            "starting OP-Succinct proof generation"
        );

        // 1. Generate witness (the expensive operation).
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
                anyhow::anyhow!("Witness generation failed: {e}")
            })?;
        let witness_gen_duration_ms = witness_start.elapsed().as_secs_f64() * 1000.0;

        let stdin_buf_count = stdin.buffer.len();
        let stdin_buf_bytes: usize = stdin.buffer.iter().map(|b| b.len()).sum();
        info!(
            witness_gen_duration_ms = witness_gen_duration_ms,
            stdin_buffers = stdin_buf_count,
            stdin_total_bytes = stdin_buf_bytes,
            range_elf_bytes = RANGE_ELF_EMBEDDED.len(),
            "witness generated, submitting to SP1 cluster"
        );

        let proof_config = ProofRequestConfig {
            cluster_rpc: cluster_rpc.clone(),
            mode: ProofMode::Compressed,
            timeout_hours: *timeout_hours,
            artifact_store: artifact_storage_config.to_cluster_config(),
        };

        // Submit to SP1 cluster.
        let cluster_proof_request = create_request(
            artifact_client.clone(),
            ClusterElf::NewElf(RANGE_ELF_EMBEDDED.to_vec()),
            stdin,
            &proof_config,
        )
        .await
        .map_err(|e| {
            error!(error = ?e, "failed to submit proof to SP1 cluster");
            anyhow::anyhow!("Failed to submit to cluster: {e:?}")
        })?;

        info!(
            proof_id = %cluster_proof_request.proof_id,
            "proof request submitted to SP1 cluster"
        );

        self.verify_artifacts(cluster_client, artifact_client, &cluster_proof_request.proof_id)
            .await;

        // 4. Build result with cluster metadata.
        let start_time_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system time before UNIX epoch")
            .as_secs();

        let metadata = json!({
            "proof_output_id": cluster_proof_request.proof_output_id.to_id(),
            "deadline_secs": cluster_proof_request.deadline
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("deadline before UNIX epoch")
                .as_secs(),
            "start_time_secs": start_time_secs,
        });

        Ok(ProveResult {
            session_id: Some(cluster_proof_request.proof_id),
            metadata: Some(metadata),
            witness_gen_duration_ms: Some(witness_gen_duration_ms),
        })
    }

    async fn process_proof_request(
        &self,
        proof_request: &ProofRequest,
        repo: &ProofRequestRepo,
    ) -> anyhow::Result<ProofProcessingResult> {
        // 1. Get all sessions for this proof request.
        let sessions = repo.get_sessions_for_request(proof_request.id).await?;

        // 2. Sync all RUNNING sessions with the SP1 cluster.
        for session in &sessions {
            if session.status == DbSessionStatus::Running
                && let Err(e) =
                    self.sync_session_with_sp1_cluster(proof_request.id, session, repo).await
            {
                warn!(
                    proof_request_id = %proof_request.id,
                    session_id = %session.backend_session_id,
                    error = %e,
                    "failed to sync session with SP1 cluster"
                );
            }
        }

        // 3. Re-query sessions to get updated state.
        let updated_sessions = repo.get_sessions_for_request(proof_request.id).await?;

        // 4. If SNARK requested, check if STARK completed and SNARK session needs
        //    to be triggered.
        if proof_request.proof_type == ProofType::OpSuccinctSp1ClusterSnarkGroth16 {
            let has_stark_completed = updated_sessions.iter().any(|s| {
                s.session_type == SessionType::Stark && s.status == DbSessionStatus::Completed
            });
            let has_snark_session =
                updated_sessions.iter().any(|s| s.session_type == SessionType::Snark);

            if has_stark_completed && !has_snark_session {
                info!(
                    proof_request_id = %proof_request.id,
                    "STARK completed, triggering stage-2 aggregation proof (SNARK Groth16)"
                );
                let fresh_proof_request = repo.get(proof_request.id).await?.ok_or_else(|| {
                    anyhow::anyhow!("Proof request not found after STARK completion")
                })?;
                if let Err(e) = self.submit_aggregation_proof(&fresh_proof_request, repo).await {
                    error!(
                        proof_request_id = %proof_request.id,
                        error = %e,
                        "failed to submit aggregation proof"
                    );
                    return Ok(ProofProcessingResult {
                        status: ProofStatus::Failed,
                        error_message: Some(format!("Failed to submit aggregation proof: {e}")),
                    });
                }
                let updated_sessions = repo.get_sessions_for_request(proof_request.id).await?;
                return Ok(Self::determine_status(proof_request.proof_type, &updated_sessions));
            }
        }

        // 5. Determine final status.
        Ok(Self::determine_status(proof_request.proof_type, &updated_sessions))
    }

    async fn get_session_status(&self, session: &ProofSession) -> anyhow::Result<SessionStatus> {
        let BackendConfig::OpSuccinct { cluster_client, .. } = &self.config else {
            unreachable!("validated in constructor");
        };

        let resp = cluster_client
            .get_proof_request(sp1_cluster_common::proto::ProofRequestGetRequest {
                proof_id: session.backend_session_id.clone(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get proof request: {e}"))?;

        resp.map_or_else(
            || Ok(SessionStatus::NotFound),
            |req| match req.proof_status() {
                ProofRequestStatus::Completed => Ok(SessionStatus::Completed),
                ProofRequestStatus::Failed => {
                    let reason = format_cluster_failure(&req);
                    Ok(SessionStatus::Failed(reason))
                }
                ProofRequestStatus::Cancelled => {
                    Ok(SessionStatus::Failed("Proof generation cancelled".to_string()))
                }
                _ => Ok(SessionStatus::Running),
            },
        )
    }

    fn name(&self) -> &'static str {
        "OP-Succinct (SP1 Cluster)"
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Build a detailed failure reason from a cluster `ProofRequest` proto.
///
/// Extracts the `execution_result` (status, failure cause, cycles) plus any
/// `metadata` / `extra_data` strings the cluster attached, producing a
/// human-readable message suitable for storage in the DB `error_message`
/// column and for structured log fields.
fn format_cluster_failure(req: &ClusterProtoProofRequest) -> String {
    let mut parts = vec!["Proof generation failed".to_string()];

    if let Some(exec) = &req.execution_result {
        let exec_status = ExecutionStatus::try_from(exec.status)
            .map(|s| s.as_str_name().to_string())
            .unwrap_or_else(|_| format!("UNKNOWN({})", exec.status));
        parts.push(format!("execution_status={exec_status}"));

        let cause = ExecutionFailureCause::try_from(exec.failure_cause)
            .map(|c| c.as_str_name().to_string())
            .unwrap_or_else(|_| format!("UNKNOWN({})", exec.failure_cause));
        if exec.failure_cause != 0 {
            parts.push(format!("failure_cause={cause}"));
        }

        if exec.cycles > 0 {
            parts.push(format!("cycles={}", exec.cycles));
        }
        if exec.gas > 0 {
            parts.push(format!("gas={}", exec.gas));
        }
    }

    if !req.metadata.is_empty() {
        parts.push(format!("metadata={}", req.metadata));
    }
    if let Some(extra) = &req.extra_data
        && !extra.is_empty()
    {
        parts.push(format!("extra_data={extra}"));
    }

    parts.join("; ")
}

impl OpSuccinctBackend {
    async fn verify_artifacts(
        &self,
        cluster_client: &sp1_cluster_common::client::ClusterServiceClient,
        artifact_client: &ArtifactClientWrapper,
        proof_id: &str,
    ) {
        use sp1_prover_types::{ArtifactClient as _, ArtifactType};

        let resp = cluster_client
            .get_proof_request(sp1_cluster_common::proto::ProofRequestGetRequest {
                proof_id: proof_id.to_string(),
            })
            .await;

        let req = match resp {
            Ok(Some(req)) => req,
            Ok(None) => {
                warn!(proof_id = %proof_id, "artifact verification: proof request not found in cluster");
                return;
            }
            Err(e) => {
                warn!(proof_id = %proof_id, error = %e, "artifact verification: failed to query cluster");
                return;
            }
        };

        let elf_id = req.program_artifact_id;
        let stdin_id = req.stdin_artifact_id;

        info!(
            proof_id = %proof_id,
            elf_artifact_id = %elf_id,
            stdin_artifact_id = %stdin_id,
            "artifact verification: attempting to download uploaded artifacts"
        );

        match artifact_client.download_raw(&elf_id, ArtifactType::Program).await {
            Ok(bytes) => {
                let len = bytes.len();
                info!(
                    proof_id = %proof_id,
                    elf_artifact_id = %elf_id,
                    downloaded_bytes = len,
                    "artifact verification: ELF download OK"
                );
            }
            Err(e) => error!(
                proof_id = %proof_id,
                elf_artifact_id = %elf_id,
                error = %e,
                "artifact verification: ELF download FAILED"
            ),
        }

        match artifact_client.download_raw(&stdin_id, ArtifactType::Stdin).await {
            Ok(bytes) => {
                let len = bytes.len();
                info!(
                    proof_id = %proof_id,
                    stdin_artifact_id = %stdin_id,
                    downloaded_bytes = len,
                    "artifact verification: stdin download OK"
                );
            }
            Err(e) => error!(
                proof_id = %proof_id,
                stdin_artifact_id = %stdin_id,
                error = %e,
                "artifact verification: stdin download FAILED"
            ),
        }
    }

    async fn sync_session_with_sp1_cluster(
        &self,
        proof_request_id: Uuid,
        session: &ProofSession,
        repo: &ProofRequestRepo,
    ) -> anyhow::Result<()> {
        let BackendConfig::OpSuccinct { artifact_client, .. } = &self.config else {
            unreachable!("validated in constructor");
        };

        let metadata = session
            .metadata
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Session metadata not found"))?;

        let proof_output_id_str = metadata
            .get("proof_output_id")
            .and_then(|v: &serde_json::Value| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("proof_output_id not found in metadata"))?;

        let proof_output_id = Artifact::from(proof_output_id_str.to_string());
        let session_status = match self.get_session_status(session).await {
            Ok(status) => status,
            Err(e) => {
                warn!(
                    proof_request_id = %proof_request_id,
                    session_id = %session.backend_session_id,
                    error = %e,
                    "transient error checking session status, will retry"
                );
                return Ok(());
            }
        };

        match session_status {
            SessionStatus::Failed(reason) => {
                error!(
                    proof_request_id = %proof_request_id,
                    session_id = %session.backend_session_id,
                    failure_detail = %reason,
                    "proof generation failed or was cancelled"
                );

                let update = UpdateProofSession {
                    backend_session_id: session.backend_session_id.clone(),
                    status: DbSessionStatus::Failed,
                    error_message: Some(reason),
                    metadata: session.metadata.clone(),
                };
                repo.update_proof_session(update).await?;

                info!(
                    proof_request_id = %proof_request_id,
                    session_id = %session.backend_session_id,
                    "session marked as failed in database"
                );
            }
            SessionStatus::Completed => {
                // Download the proof artifact directly from the artifact store.
                // We already confirmed completion via gRPC, so we bypass the
                // upstream `check_proof_status` which would reject expired
                // deadlines and re-query cluster status redundantly.
                let ac = artifact_client.clone();
                let poi = proof_output_id.clone();
                let download_result = tokio::spawn(async move {
                    ac.download_with_type::<ProofFromNetwork>(&poi, ArtifactType::Proof).await
                })
                .await;

                let proof: ProofFromNetwork = match download_result {
                    Ok(Ok(proof)) => proof,
                    Ok(Err(e)) => {
                        error!(
                            proof_request_id = %proof_request_id,
                            session_id = %session.backend_session_id,
                            error = %e,
                            "failed to download proof artifact for completed session"
                        );
                        return Ok(());
                    }
                    Err(join_err) => {
                        warn!(
                            proof_request_id = %proof_request_id,
                            session_id = %session.backend_session_id,
                            error = %join_err,
                            "proof artifact download panicked, will retry next poll"
                        );
                        return Ok(());
                    }
                };

                let total_elapsed_secs =
                    metadata.get("start_time_secs").and_then(|v| v.as_u64()).map(|start| {
                        SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .expect("system time before UNIX epoch")
                            .as_secs()
                            - start
                    });

                info!(
                    proof_request_id = %proof_request_id,
                    session_id = %session.backend_session_id,
                    total_elapsed_secs = ?total_elapsed_secs,
                    "proof completed, downloaded from artifact store"
                );

                let proof_with_pv: SP1ProofWithPublicValues = proof.into();
                let proof_bytes =
                    bincode::serde::encode_to_vec(&proof_with_pv, bincode::config::standard())?;

                let mut updated_metadata = session.metadata.clone().unwrap_or_else(|| json!({}));
                if let Some(obj) = updated_metadata.as_object_mut()
                    && let Some(total) = total_elapsed_secs
                {
                    obj.insert("total_elapsed_secs".to_string(), json!(total));
                }

                let update = UpdateProofSession {
                    backend_session_id: session.backend_session_id.clone(),
                    status: DbSessionStatus::Completed,
                    error_message: None,
                    metadata: Some(updated_metadata),
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
            SessionStatus::Running => {
                info!(
                    proof_request_id = %proof_request_id,
                    session_id = %session.backend_session_id,
                    "proof still running"
                );
            }
            SessionStatus::NotFound => {
                warn!(
                    proof_request_id = %proof_request_id,
                    session_id = %session.backend_session_id,
                    "session not found in cluster, will retry"
                );
            }
        }

        Ok(())
    }

    /// Determine final status based on sessions and proof type.
    fn determine_status(proof_type: ProofType, sessions: &[ProofSession]) -> ProofProcessingResult {
        if sessions.is_empty() {
            return ProofProcessingResult { status: ProofStatus::Pending, error_message: None };
        }

        // Any failure → FAILED.
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
                let all_succeeded = sessions.iter().all(|s| s.status == DbSessionStatus::Completed);
                if all_succeeded {
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

    /// Submit an aggregation proof (stage 2) after the STARK proof completes.
    async fn submit_aggregation_proof(
        &self,
        proof_request: &ProofRequest,
        repo: &ProofRequestRepo,
    ) -> anyhow::Result<()> {
        let BackendConfig::OpSuccinct {
            cluster_rpc,
            artifact_client,
            artifact_storage_config,
            timeout_hours,
            range_vk,
            ..
        } = &self.config
        else {
            unreachable!("validated in constructor");
        };

        let prover_address: alloy_primitives::Address = proof_request
            .prover_address
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("prover_address required for aggregation"))?
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid prover_address in DB: {e}"))?;

        // 1. Deserialize STARK proof.
        let stark_receipt_bytes = proof_request
            .stark_receipt
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("STARK receipt not found for aggregation"))?;

        let (stark_proof_with_pv, _): (SP1ProofWithPublicValues, _) =
            bincode::serde::decode_from_slice(stark_receipt_bytes, bincode::config::standard())
                .map_err(|e| anyhow::anyhow!("Failed to deserialize STARK proof: {e}"))?;

        // 2. Extract boot_info from public values.
        let mut public_values = stark_proof_with_pv.public_values.clone();
        let boot_info: base_proof_succinct_client_utils::boot::BootInfoStruct =
            public_values.read();
        let boot_infos = vec![boot_info];
        let proofs = vec![stark_proof_with_pv.proof];

        // 3. Fetch L1 headers for aggregation.
        let fetcher = self.provider.fetcher();
        let header = fetcher
            .get_latest_l1_head_in_batch(&boot_infos)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get latest L1 head in batch: {e}"))?;
        let l1_head_hash = header.hash_slow();

        let headers = fetcher
            .get_header_preimages(&boot_infos, l1_head_hash)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get header preimages: {e}"))?;

        info!(
            proof_request_id = %proof_request.id,
            l1_head_hash = %l1_head_hash,
            num_headers = headers.len(),
            "fetched L1 headers for aggregation proof"
        );

        // 4. Build aggregation stdin.
        let stdin = get_agg_proof_stdin(
            proofs,
            boot_infos,
            headers,
            range_vk,
            l1_head_hash,
            prover_address,
        )
        .map_err(|e| anyhow::anyhow!("Failed to build aggregation stdin: {e}"))?;

        // 5. Submit aggregation ELF with Groth16 mode.
        let proof_config = ProofRequestConfig {
            cluster_rpc: cluster_rpc.clone(),
            mode: ProofMode::Groth16,
            timeout_hours: *timeout_hours,
            artifact_store: artifact_storage_config.to_cluster_config(),
        };

        let cluster_proof_request = create_request(
            artifact_client.clone(),
            ClusterElf::NewElf(AGGREGATION_ELF.to_vec()),
            stdin,
            &proof_config,
        )
        .await
        .map_err(|e| {
            error!(error = %e, "failed to submit aggregation proof to SP1 cluster");
            anyhow::anyhow!("Failed to submit aggregation proof to cluster: {e}")
        })?;

        info!(
            proof_request_id = %proof_request.id,
            proof_id = %cluster_proof_request.proof_id,
            "aggregation proof (Groth16) submitted to SP1 cluster"
        );

        // 6. Create SNARK session in DB.
        let start_time_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system time before UNIX epoch")
            .as_secs();

        let metadata = json!({
            "proof_output_id": cluster_proof_request.proof_output_id.to_id(),
            "deadline_secs": cluster_proof_request.deadline
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("deadline before UNIX epoch")
                .as_secs(),
            "start_time_secs": start_time_secs,
        });

        let session = CreateProofSession {
            proof_request_id: proof_request.id,
            session_type: SessionType::Snark,
            backend_session_id: cluster_proof_request.proof_id,
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
            proof_request_id: Uuid::new_v4(),
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
            OpSuccinctBackend::determine_status(ProofType::OpSuccinctSp1ClusterCompressed, &[]);
        assert_eq!(result.status, ProofStatus::Pending);
        assert!(result.error_message.is_none());
    }

    #[test]
    fn test_determine_status_no_sessions_snark_returns_pending() {
        let result =
            OpSuccinctBackend::determine_status(ProofType::OpSuccinctSp1ClusterSnarkGroth16, &[]);
        assert_eq!(result.status, ProofStatus::Pending);
    }

    #[test]
    fn test_determine_status_compressed_all_completed_returns_succeeded() {
        let sessions = vec![make_session(SessionType::Stark, DbSessionStatus::Completed)];
        let result = OpSuccinctBackend::determine_status(
            ProofType::OpSuccinctSp1ClusterCompressed,
            &sessions,
        );
        assert_eq!(result.status, ProofStatus::Succeeded);
        assert!(result.error_message.is_none());
    }

    #[test]
    fn test_determine_status_compressed_running_returns_running() {
        let sessions = vec![make_session(SessionType::Stark, DbSessionStatus::Running)];
        let result = OpSuccinctBackend::determine_status(
            ProofType::OpSuccinctSp1ClusterCompressed,
            &sessions,
        );
        assert_eq!(result.status, ProofStatus::Running);
    }

    #[test]
    fn test_determine_status_compressed_failed_returns_failed() {
        let sessions = vec![make_failed_session(SessionType::Stark, "OOM")];
        let result = OpSuccinctBackend::determine_status(
            ProofType::OpSuccinctSp1ClusterCompressed,
            &sessions,
        );
        assert_eq!(result.status, ProofStatus::Failed);
        assert_eq!(result.error_message.as_deref(), Some("OOM"));
    }

    #[test]
    fn test_determine_status_snark_both_completed_returns_succeeded() {
        let sessions = vec![
            make_session(SessionType::Stark, DbSessionStatus::Completed),
            make_session(SessionType::Snark, DbSessionStatus::Completed),
        ];
        let result = OpSuccinctBackend::determine_status(
            ProofType::OpSuccinctSp1ClusterSnarkGroth16,
            &sessions,
        );
        assert_eq!(result.status, ProofStatus::Succeeded);
    }

    #[test]
    fn test_determine_status_snark_only_stark_completed_returns_running() {
        let sessions = vec![make_session(SessionType::Stark, DbSessionStatus::Completed)];
        let result = OpSuccinctBackend::determine_status(
            ProofType::OpSuccinctSp1ClusterSnarkGroth16,
            &sessions,
        );
        assert_eq!(result.status, ProofStatus::Running);
    }

    #[test]
    fn test_determine_status_snark_stark_completed_snark_running_returns_running() {
        let sessions = vec![
            make_session(SessionType::Stark, DbSessionStatus::Completed),
            make_session(SessionType::Snark, DbSessionStatus::Running),
        ];
        let result = OpSuccinctBackend::determine_status(
            ProofType::OpSuccinctSp1ClusterSnarkGroth16,
            &sessions,
        );
        assert_eq!(result.status, ProofStatus::Running);
    }

    #[test]
    fn test_determine_status_snark_stark_failed_returns_failed() {
        let sessions = vec![make_failed_session(SessionType::Stark, "cluster timeout")];
        let result = OpSuccinctBackend::determine_status(
            ProofType::OpSuccinctSp1ClusterSnarkGroth16,
            &sessions,
        );
        assert_eq!(result.status, ProofStatus::Failed);
        assert_eq!(result.error_message.as_deref(), Some("cluster timeout"));
    }

    #[test]
    fn test_determine_status_snark_snark_failed_returns_failed() {
        let sessions = vec![
            make_session(SessionType::Stark, DbSessionStatus::Completed),
            make_failed_session(SessionType::Snark, "aggregation error"),
        ];
        let result = OpSuccinctBackend::determine_status(
            ProofType::OpSuccinctSp1ClusterSnarkGroth16,
            &sessions,
        );
        assert_eq!(result.status, ProofStatus::Failed);
        assert_eq!(result.error_message.as_deref(), Some("aggregation error"));
    }

    #[test]
    fn test_determine_status_failure_takes_priority_over_completed() {
        let sessions = vec![
            make_session(SessionType::Stark, DbSessionStatus::Completed),
            make_failed_session(SessionType::Snark, "failure wins"),
        ];
        let result = OpSuccinctBackend::determine_status(
            ProofType::OpSuccinctSp1ClusterCompressed,
            &sessions,
        );
        assert_eq!(result.status, ProofStatus::Failed);
    }
}
