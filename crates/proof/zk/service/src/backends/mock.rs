//! Mock proving backend for testing.
//!
//! Produces instant fake proofs using SP1's mock prover APIs.
//! Enabled when `SP1_PROVER=mock` is set. No witness generation, no cluster,
//! no S3 — everything runs locally in milliseconds.

use std::{collections::HashMap, sync::Mutex};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_proof_succinct_client_utils::boot::BootInfoStruct;
use base_zk_client::ProveBlockRequest;
use base_zk_db::{
    CreateProofSession, ProofRequest, ProofRequestRepo, ProofSession, ProofStatus, ProofType,
    SessionStatus as DbSessionStatus, SessionType, UpdateReceipt,
};
use serde_json::json;
use sp1_sdk::{
    SP1_CIRCUIT_VERSION, SP1ProofMode, SP1ProofWithPublicValues, SP1PublicValues, SP1VerifyingKey,
};
use tracing::info;
use uuid::Uuid;

use super::traits::{
    BackendType, ProofProcessingResult, ProveResult, ProvingBackend, SessionStatus,
};

/// Mock backend that produces instant fake proofs.
pub struct MockBackend {
    range_vk: SP1VerifyingKey,
    agg_vk: SP1VerifyingKey,
    /// Tracks mock session IDs that have been created.
    sessions: Mutex<HashMap<String, ()>>,
}

impl std::fmt::Debug for MockBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockBackend").finish_non_exhaustive()
    }
}

impl MockBackend {
    /// Create a new mock backend with the given verification keys.
    pub fn new(range_vk: SP1VerifyingKey, agg_vk: SP1VerifyingKey) -> Self {
        Self { range_vk, agg_vk, sessions: Mutex::new(HashMap::new()) }
    }

    /// Build synthetic [`BootInfoStruct`] public values from request parameters.
    fn build_mock_public_values(request: &ProveBlockRequest) -> SP1PublicValues {
        let start_block = request.start_block_number;
        let end_block = start_block + request.number_of_blocks_to_prove;

        let boot_info = BootInfoStruct {
            l1Head: B256::repeat_byte(0x11),
            l2PreRoot: B256::repeat_byte(0x22),
            l2PostRoot: B256::repeat_byte(0x33),
            l2PreBlockNumber: start_block,
            l2BlockNumber: end_block,
            rollupConfigHash: B256::repeat_byte(0x44),
            intermediateRoots: Default::default(),
        };

        let mut pv = SP1PublicValues::new();
        pv.write(&boot_info);
        pv
    }

    /// Create a mock STARK (compressed) proof.
    fn create_mock_stark_proof(&self, request: &ProveBlockRequest) -> Vec<u8> {
        let pv = Self::build_mock_public_values(request);
        let proof = SP1ProofWithPublicValues::create_mock_proof(
            &self.range_vk,
            pv,
            SP1ProofMode::Compressed,
            SP1_CIRCUIT_VERSION,
        );
        bincode::serde::encode_to_vec(&proof, bincode::config::standard())
            .expect("failed to bincode-serialize mock proof")
    }

    /// Create a mock SNARK (Groth16) proof.
    fn create_mock_snark_proof(&self) -> Vec<u8> {
        let pv = SP1PublicValues::from(&[0u8; 32]);
        let proof = SP1ProofWithPublicValues::create_mock_proof(
            &self.agg_vk,
            pv,
            SP1ProofMode::Groth16,
            SP1_CIRCUIT_VERSION,
        );
        bincode::serde::encode_to_vec(&proof, bincode::config::standard())
            .expect("failed to bincode-serialize mock SNARK proof")
    }
}

#[async_trait]
impl ProvingBackend for MockBackend {
    fn backend_type(&self) -> BackendType {
        BackendType::OpSuccinct
    }

    async fn prove(&self, request: &ProveBlockRequest) -> anyhow::Result<ProveResult> {
        if request.number_of_blocks_to_prove == 0 {
            anyhow::bail!("number_of_blocks_to_prove must be > 0");
        }

        let session_id = format!("mock-{}", Uuid::new_v4());

        info!(
            session_id = %session_id,
            start_block = request.start_block_number,
            num_blocks = request.number_of_blocks_to_prove,
            "MockBackend: created instant STARK proof"
        );

        self.sessions.lock().unwrap().insert(session_id.clone(), ());

        let metadata = json!({
            "mock": true,
            "proof_output_id": format!("mock-output-{}", Uuid::new_v4()),
            "deadline_secs": 0u64,
            "start_time_secs": 0u64,
        });

        Ok(ProveResult {
            session_id: Some(session_id),
            metadata: Some(metadata),
            witness_gen_duration_ms: None,
        })
    }

    async fn process_proof_request(
        &self,
        proof_request: &ProofRequest,
        repo: &ProofRequestRepo,
    ) -> anyhow::Result<ProofProcessingResult> {
        let sessions = repo.get_sessions_for_request(proof_request.id).await?;

        if sessions.is_empty() {
            return Ok(ProofProcessingResult { status: ProofStatus::Pending, error_message: None });
        }

        // Check for failures.
        for session in &sessions {
            if session.status == DbSessionStatus::Failed {
                return Ok(ProofProcessingResult {
                    status: ProofStatus::Failed,
                    error_message: session.error_message.clone(),
                });
            }
        }

        // For running sessions, instantly complete them with mock proofs.
        for session in &sessions {
            if session.status == DbSessionStatus::Running {
                let proof_bytes = match session.session_type {
                    SessionType::Stark => {
                        let request = ProveBlockRequest {
                            start_block_number: proof_request.start_block_number as u64,
                            number_of_blocks_to_prove: proof_request.number_of_blocks_to_prove
                                as u64,
                            sequence_window: proof_request.sequence_window.map(|w| w as u64),
                            proof_type: match proof_request.proof_type {
                                ProofType::OpSuccinctSp1ClusterCompressed => 3,
                                ProofType::OpSuccinctSp1ClusterSnarkGroth16 => 4,
                            },
                            session_id: None,
                            prover_address: proof_request.prover_address.clone(),
                            l1_head: proof_request.l1_head.clone(),
                        };
                        self.create_mock_stark_proof(&request)
                    }
                    SessionType::Snark => self.create_mock_snark_proof(),
                };

                let update_receipt = match session.session_type {
                    SessionType::Stark => UpdateReceipt {
                        id: proof_request.id,
                        stark_receipt: Some(proof_bytes),
                        snark_receipt: None,
                        status: ProofStatus::Running,
                        error_message: None,
                    },
                    SessionType::Snark => UpdateReceipt {
                        id: proof_request.id,
                        stark_receipt: None,
                        snark_receipt: Some(proof_bytes),
                        status: ProofStatus::Running,
                        error_message: None,
                    },
                };

                repo.complete_session_and_update_receipt(
                    &session.backend_session_id,
                    update_receipt,
                )
                .await?;

                info!(
                    proof_request_id = %proof_request.id,
                    session_id = %session.backend_session_id,
                    session_type = %session.session_type,
                    "MockBackend: instantly completed session"
                );
            }
        }

        // Re-query sessions after updates.
        let updated_sessions = repo.get_sessions_for_request(proof_request.id).await?;

        // For SNARK_GROTH16, check if STARK is done and SNARK session needs creation.
        if proof_request.proof_type == ProofType::OpSuccinctSp1ClusterSnarkGroth16 {
            let has_stark_completed = updated_sessions.iter().any(|s| {
                s.session_type == SessionType::Stark && s.status == DbSessionStatus::Completed
            });
            let has_snark_session =
                updated_sessions.iter().any(|s| s.session_type == SessionType::Snark);

            if has_stark_completed && !has_snark_session {
                info!(
                    proof_request_id = %proof_request.id,
                    "MockBackend: STARK done, creating SNARK session"
                );

                let snark_session_id = format!("mock-snark-{}", Uuid::new_v4());
                let metadata = json!({
                    "mock": true,
                    "proof_output_id": format!("mock-snark-output-{}", Uuid::new_v4()),
                    "deadline_secs": 0u64,
                    "start_time_secs": 0u64,
                });

                let session = CreateProofSession {
                    proof_request_id: proof_request.id,
                    session_type: SessionType::Snark,
                    backend_session_id: snark_session_id,
                    metadata: Some(metadata),
                };

                repo.create_proof_session(session).await?;

                let final_sessions = repo.get_sessions_for_request(proof_request.id).await?;
                return Ok(determine_mock_status(proof_request.proof_type, &final_sessions));
            }
        }

        Ok(determine_mock_status(proof_request.proof_type, &updated_sessions))
    }

    async fn get_session_status(&self, _session: &ProofSession) -> anyhow::Result<SessionStatus> {
        Ok(SessionStatus::Completed)
    }

    fn name(&self) -> &'static str {
        "Mock (instant fake proofs)"
    }
}

/// Determine status from sessions.
fn determine_mock_status(
    proof_type: ProofType,
    sessions: &[ProofSession],
) -> ProofProcessingResult {
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
            ProofProcessingResult {
                status: if all_completed { ProofStatus::Succeeded } else { ProofStatus::Running },
                error_message: None,
            }
        }
        ProofType::OpSuccinctSp1ClusterSnarkGroth16 => {
            let stark_done = sessions.iter().any(|s| {
                s.session_type == SessionType::Stark && s.status == DbSessionStatus::Completed
            });
            let snark_done = sessions.iter().any(|s| {
                s.session_type == SessionType::Snark && s.status == DbSessionStatus::Completed
            });
            ProofProcessingResult {
                status: if stark_done && snark_done {
                    ProofStatus::Succeeded
                } else {
                    ProofStatus::Running
                },
                error_message: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_stark_proof_roundtrip() {
        let request = ProveBlockRequest {
            start_block_number: 100,
            number_of_blocks_to_prove: 5,
            sequence_window: Some(50),
            proof_type: 3,
            session_id: None,
            prover_address: None,
            l1_head: None,
        };

        let pv = MockBackend::build_mock_public_values(&request);
        let bytes = pv.as_slice();
        assert!(!bytes.is_empty(), "public values should not be empty");

        let mut pv_clone = SP1PublicValues::from(bytes);
        let boot_info: BootInfoStruct = pv_clone.read();
        assert_eq!(boot_info.l2PreBlockNumber, 100);
        assert_eq!(boot_info.l2BlockNumber, 105);
        assert_eq!(boot_info.l1Head, B256::repeat_byte(0x11));
    }
}
