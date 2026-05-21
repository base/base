//! Dry-run backend for local SP1 execution statistics.
//!
//! This backend generates a real witness and executes the range program with
//! `MockProver`, but it does not produce or submit a proof.

use std::{collections::HashMap, fmt};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_proof_succinct_client_utils::client::DEFAULT_INTERMEDIATE_ROOT_INTERVAL;
use base_proof_succinct_proof_utils::get_range_elf_embedded;
use base_zk_client::{ExecutionStats, ProveBlockRequest};
use base_zk_db::{
    ProofRequest, ProofRequestRepo, ProofSession, ProofStatus, ProofType,
    SessionStatus as DbSessionStatus, UpdateProofSession,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sp1_sdk::{
    Elf, SP1Stdin,
    blocking::{MockProver, Prover},
};
use tracing::{error, info, warn};
use uuid::Uuid;

use super::provider::{OpSuccinctProvider, WitnessParams};
use crate::backends::traits::{
    BackendType, ProofProcessingResult, ProveResult, ProvingBackend, SessionStatus,
};

/// Metadata key where dry-run execution stats are stored on proof sessions.
pub const EXECUTION_STATS_METADATA_KEY: &str = "execution_stats";

/// Metadata key indicating that a session was produced by the dry-run backend.
pub const DRY_RUN_METADATA_KEY: &str = "dry_run";

/// Local execution backend that returns SP1 execution statistics.
#[derive(Clone)]
pub struct DryRunBackend {
    provider: OpSuccinctProvider,
    base_consensus_url: String,
    l1_node_url: String,
    default_sequence_window: u64,
}

/// Execution statistics persisted in proof-session metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredExecutionStats {
    /// Total RISC-V instruction cycles reported by SP1.
    pub total_instruction_cycles: u64,
    /// Total SP1 gas reported by SP1.
    pub total_sp1_gas: u64,
    /// Per-section cycle tracker values reported by the range program.
    pub cycle_tracker: HashMap<String, u64>,
    /// Time spent generating the witness, in milliseconds.
    pub witness_generation_ms: f64,
    /// Time spent executing the SP1 range program, in milliseconds.
    pub execution_ms: f64,
}

impl From<ExecutionStats> for StoredExecutionStats {
    fn from(value: ExecutionStats) -> Self {
        Self {
            total_instruction_cycles: value.total_instruction_cycles,
            total_sp1_gas: value.total_sp1_gas,
            cycle_tracker: value.cycle_tracker,
            witness_generation_ms: value.witness_generation_ms,
            execution_ms: value.execution_ms,
        }
    }
}

impl From<StoredExecutionStats> for ExecutionStats {
    fn from(value: StoredExecutionStats) -> Self {
        Self {
            total_instruction_cycles: value.total_instruction_cycles,
            total_sp1_gas: value.total_sp1_gas,
            cycle_tracker: value.cycle_tracker,
            witness_generation_ms: value.witness_generation_ms,
            execution_ms: value.execution_ms,
        }
    }
}

impl fmt::Debug for DryRunBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DryRunBackend").finish_non_exhaustive()
    }
}

impl DryRunBackend {
    /// Create a dry-run backend using the shared OP Succinct witness provider.
    pub const fn new(
        provider: OpSuccinctProvider,
        base_consensus_url: String,
        l1_node_url: String,
        default_sequence_window: u64,
    ) -> Self {
        Self { provider, base_consensus_url, l1_node_url, default_sequence_window }
    }

    async fn execute_range_program(stdin: SP1Stdin) -> anyhow::Result<ExecutionStats> {
        let execution_start = std::time::Instant::now();
        let (_, report) = tokio::task::spawn_blocking(move || {
            info!("starting local SP1 zkVM execution");

            let prover = MockProver::new();
            prover
                .execute(Elf::Static(get_range_elf_embedded()), stdin)
                .calculate_gas(true)
                .deferred_proof_verification(false)
                .run()
        })
        .await
        .map_err(|e| anyhow::anyhow!("SP1 execution task failed to join: {e}"))??;

        let execution_ms = execution_start.elapsed().as_secs_f64() * 1000.0;
        let stats = ExecutionStats {
            total_instruction_cycles: report.total_instruction_count(),
            total_sp1_gas: report.gas().unwrap_or_else(|| {
                warn!("gas calculation returned None despite calculate_gas(true)");
                0
            }),
            cycle_tracker: report.cycle_tracker.into_iter().collect(),
            witness_generation_ms: 0.0,
            execution_ms,
        };

        Ok(stats)
    }
}

#[async_trait]
impl ProvingBackend for DryRunBackend {
    fn backend_type(&self) -> BackendType {
        BackendType::OpSuccinct
    }

    async fn prove(&self, request: &ProveBlockRequest) -> anyhow::Result<ProveResult> {
        if request.number_of_blocks_to_prove == 0 {
            anyhow::bail!("number_of_blocks_to_prove must be > 0");
        }

        let proof_type = ProofType::try_from(request.proof_type)
            .map_err(|e| anyhow::anyhow!("invalid proof_type: {e}"))?;
        if proof_type == ProofType::OpSuccinctSp1ClusterSnarkGroth16 {
            anyhow::bail!(
                "dry-run backend only supports compressed proof types; SNARK_GROTH16 requires a proof-producing backend"
            );
        }

        let start_block = request.start_block_number;
        let num_blocks = request.number_of_blocks_to_prove;
        let end_block = start_block.checked_add(num_blocks).ok_or_else(|| {
            anyhow::anyhow!("block range overflow: start={start_block} + count={num_blocks}")
        })?;
        let sequence_window = request.sequence_window.unwrap_or(self.default_sequence_window);
        let intermediate_root_interval =
            request.intermediate_root_interval.unwrap_or(DEFAULT_INTERMEDIATE_ROOT_INTERVAL);
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
            intermediate_root_interval = intermediate_root_interval,
            l1_head = ?l1_head,
            "starting dry-run SP1 execution"
        );

        let witness_start = std::time::Instant::now();
        let stdin = self
            .provider
            .generate_witness(WitnessParams {
                start_block,
                end_block,
                sequence_window,
                l1_node_url: &self.l1_node_url,
                base_consensus_url: &self.base_consensus_url,
                l1_head,
                intermediate_root_interval,
            })
            .await
            .map_err(|e| {
                error!(
                    start_block = start_block,
                    end_block = end_block,
                    error = %e,
                    "dry-run witness generation failed"
                );
                anyhow::anyhow!("witness generation failed: {e}")
            })?;
        let witness_generation_ms = witness_start.elapsed().as_secs_f64() * 1000.0;

        let mut execution_stats = Self::execute_range_program(stdin).await?;
        execution_stats.witness_generation_ms = witness_generation_ms;

        info!(
            total_instruction_cycles = execution_stats.total_instruction_cycles,
            total_sp1_gas = execution_stats.total_sp1_gas,
            witness_generation_ms = witness_generation_ms,
            execution_ms = execution_stats.execution_ms,
            tracked_sections = execution_stats.cycle_tracker.len(),
            "dry-run SP1 execution completed"
        );

        let session_id = format!("dry-run-{}", Uuid::new_v4());
        let stored_stats = StoredExecutionStats::from(execution_stats);
        let metadata = json!({
            DRY_RUN_METADATA_KEY: true,
            EXECUTION_STATS_METADATA_KEY: stored_stats,
        });

        Ok(ProveResult {
            session_id: Some(session_id),
            metadata: Some(metadata),
            witness_gen_duration_ms: Some(witness_generation_ms),
        })
    }

    async fn process_proof_request(
        &self,
        proof_request: &ProofRequest,
        repo: &ProofRequestRepo,
    ) -> anyhow::Result<ProofProcessingResult> {
        if proof_request.proof_type == ProofType::OpSuccinctSp1ClusterSnarkGroth16 {
            return Ok(ProofProcessingResult {
                status: ProofStatus::Failed,
                error_message: Some(
                    "dry-run backend only supports compressed proof types; SNARK_GROTH16 requires a proof-producing backend"
                        .to_string(),
                ),
            });
        }

        let sessions = repo.get_sessions_for_request(proof_request.id).await?;

        if sessions.is_empty() {
            return Ok(ProofProcessingResult { status: ProofStatus::Pending, error_message: None });
        }

        for session in &sessions {
            if session.status == DbSessionStatus::Failed {
                return Ok(ProofProcessingResult {
                    status: ProofStatus::Failed,
                    error_message: session.error_message.clone(),
                });
            }
        }

        let running_sessions =
            sessions.iter().filter(|session| session.status == DbSessionStatus::Running);

        for session in running_sessions {
            let updated = repo
                .update_proof_session_if_non_terminal(UpdateProofSession {
                    backend_session_id: session.backend_session_id.clone(),
                    status: DbSessionStatus::Completed,
                    error_message: None,
                    metadata: None,
                })
                .await?;

            if updated {
                info!(
                    proof_request_id = %proof_request.id,
                    session_id = %session.backend_session_id,
                    "dry-run session completed"
                );
            }
        }

        let updated_sessions = repo.get_sessions_for_request(proof_request.id).await?;
        let all_complete = updated_sessions.iter().all(|s| s.status == DbSessionStatus::Completed);
        let status = if all_complete { ProofStatus::Succeeded } else { ProofStatus::Running };

        Ok(ProofProcessingResult { status, error_message: None })
    }

    async fn get_session_status(&self, _session: &ProofSession) -> anyhow::Result<SessionStatus> {
        Ok(SessionStatus::Completed)
    }

    fn name(&self) -> &'static str {
        "Dry-run (local SP1 execution stats)"
    }
}
