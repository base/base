//! Dry-run [`ZkProver`] for local SP1 execution statistics.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use base_proof_zk_host::{ZkProver, ZkProverError, ZkSessionState};
use base_prover_service_protocol::{
    ExecutionStats, ProofResult, SessionType, SnarkPlonkProofRequest, SnarkPlonkProofResult,
    ZkProofRequest, ZkProofResult, ZkVm,
};
use sp1_sdk::{
    Elf, SP1Stdin,
    blocking::{LightProver, Prover},
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::utils::get_range_elf_embedded;
use crate::succinct::{
    OpSuccinctWitnessProvider, SuccinctRpcConfig, SuccinctZkProverBuildError,
    SuccinctZkProverBuilder,
};

macro_rules! backend_error {
    ($($arg:tt)*) => {
        ZkProverError::Backend(std::io::Error::other(format!($($arg)*)).into())
    };
}

/// Prefix marking a dry-run STARK (range) backend session id.
pub const DRY_RUN_STARK_PREFIX: &str = "dry-run-stark-";

/// Prefix marking a dry-run SNARK (aggregation) backend session id.
pub const DRY_RUN_SNARK_PREFIX: &str = "dry-run-snark-";

/// [`ZkProver`] that generates a witness and executes the range program locally.
#[derive(Clone, Debug)]
pub struct DryRunZkProver {
    provider: OpSuccinctWitnessProvider,
    range_cycle_limit: u64,
    completed_results: Arc<Mutex<HashMap<String, ProofResult>>>,
}

impl DryRunZkProver {
    /// Create a dry-run prover with a witness provider and range cycle limit.
    pub fn new(provider: OpSuccinctWitnessProvider, range_cycle_limit: u64) -> Self {
        Self {
            provider,
            range_cycle_limit,
            completed_results: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Builds a dry-run backend.
    pub async fn build_until_cancelled(
        rpc: SuccinctRpcConfig,
        range_cycle_limit: u64,
        witness_provider: Option<OpSuccinctWitnessProvider>,
        cancel: &CancellationToken,
    ) -> Result<Option<Arc<dyn ZkProver>>, SuccinctZkProverBuildError> {
        info!(backend = "dry_run", "using local SP1 dry-run backend");
        let provider = match witness_provider {
            Some(provider) => provider,
            None => {
                let Some(provider) =
                    SuccinctZkProverBuilder::build_witness_provider(rpc, cancel).await?
                else {
                    return Ok(None);
                };
                provider
            }
        };

        Ok(Some(Arc::new(Self::new(provider, range_cycle_limit))))
    }

    /// Execute the range program with SP1's light prover and return execution statistics.
    ///
    /// Cancellation note: SP1's blocking execution is not interruptible once dispatched. Dropping
    /// this future does not stop an already-running local execution task.
    pub async fn execute_range_program(
        stdin: SP1Stdin,
        range_cycle_limit: u64,
    ) -> Result<ExecutionStats, ZkProverError> {
        let (execution_result, execution_ms) = tokio::task::spawn_blocking(move || {
            info!(range_cycle_limit = range_cycle_limit, "starting local SP1 zkVM execution");

            let execution_start = std::time::Instant::now();
            let prover = LightProver::new();
            let result = prover
                .execute(Elf::Static(get_range_elf_embedded()), stdin)
                .cycle_limit(range_cycle_limit)
                .calculate_gas(true)
                .deferred_proof_verification(false)
                .run();
            let execution_ms =
                u64::try_from(execution_start.elapsed().as_millis()).unwrap_or(u64::MAX);

            (result, execution_ms)
        })
        .await
        .map_err(|e| backend_error!("SP1 execution task failed to join: {e}"))?;
        let (_, report) =
            execution_result.map_err(|e| backend_error!("SP1 execution failed: {e}"))?;

        Ok(ExecutionStats {
            total_instruction_cycles: report.total_instruction_count(),
            total_sp1_gas: report.gas().unwrap_or_else(|| {
                warn!("gas calculation returned None despite calculate_gas(true)");
                0
            }),
            cycle_tracker: report.cycle_tracker.into_iter().collect(),
            witness_generation_ms: 0,
            execution_ms,
        })
    }

    /// Generate the witness, execute it locally, and return an empty-proof dry-run result.
    pub async fn prove_range(
        &self,
        request: &ZkProofRequest,
        request_session_id: &str,
    ) -> Result<ProofResult, ZkProverError> {
        let witness_start = std::time::Instant::now();
        let stdin = self
            .provider
            .generate_range_witness(request)
            .await
            .map_err(|e| backend_error!("witness generation failed: {e}"))?;

        let witness_generation_ms =
            u64::try_from(witness_start.elapsed().as_millis()).unwrap_or(u64::MAX);

        info!(request_session_id = %request_session_id, "starting dry-run SP1 execution");

        let mut execution_stats =
            Self::execute_range_program(stdin, self.range_cycle_limit).await?;
        // Histogram latency is recorded in the provider; this field keeps the dry-run API contract.
        execution_stats.witness_generation_ms = witness_generation_ms;

        info!(
            request_session_id = %request_session_id,
            total_instruction_cycles = execution_stats.total_instruction_cycles,
            total_sp1_gas = execution_stats.total_sp1_gas,
            execution_ms = execution_stats.execution_ms,
            tracked_sections = execution_stats.cycle_tracker.len(),
            "dry-run SP1 execution completed"
        );

        Ok(ProofResult::Compressed(ZkProofResult {
            zk_vm: ZkVm::Sp1,
            proof: Vec::new().into(),
            execution_stats: Some(execution_stats),
        }))
    }

    /// Deterministic backend session id for a request's initial range (STARK) stage.
    pub fn backend_session_id(request_session_id: &str) -> String {
        Self::backend_session_id_for_type(SessionType::Stark, request_session_id)
    }

    /// Deterministic backend session id for a given stage.
    pub fn backend_session_id_for_type(
        session_type: SessionType,
        request_session_id: &str,
    ) -> String {
        match session_type {
            SessionType::Stark => format!("{DRY_RUN_STARK_PREFIX}{request_session_id}"),
            SessionType::Snark => format!("{DRY_RUN_SNARK_PREFIX}{request_session_id}"),
        }
    }

    /// Swap a completed range result for its aggregation (SNARK) result under
    /// `backend_session_id`, returning that id. The lock is held across the whole check-remove-
    /// insert so the swap is atomic and idempotent on crash-resume.
    pub fn advance_range_to_snark(
        completed_results: &Mutex<HashMap<String, ProofResult>>,
        backend_session_id: String,
        completed_backend_session_id: &str,
    ) -> Result<String, ZkProverError> {
        let mut store = completed_results
            .lock()
            .map_err(|e| backend_error!("dry-run result store lock poisoned: {e}"))?;

        if store.contains_key(&backend_session_id) {
            return Ok(backend_session_id);
        }

        let range_result = store.remove(completed_backend_session_id).ok_or_else(|| {
            ZkProverError::BackendSessionNotFound {
                backend_session_id: completed_backend_session_id.to_owned(),
            }
        })?;
        let execution_stats = match range_result {
            ProofResult::Compressed(proof) => proof.execution_stats,
            _ => return Err(backend_error!("dry-run range result had unexpected proof type")),
        };
        let result = ProofResult::SnarkPlonk(SnarkPlonkProofResult {
            proof: ZkProofResult { zk_vm: ZkVm::Sp1, proof: Vec::new().into(), execution_stats },
        });
        store.insert(backend_session_id.clone(), result);

        Ok(backend_session_id)
    }
}

/// Dry-run submission is synchronous: `submit` runs local SP1 execution before returning a
/// completed backend session id. Results are process-local and non-durable.
#[async_trait]
impl ZkProver for DryRunZkProver {
    async fn submit(
        &self,
        request: &ZkProofRequest,
        request_session_id: &str,
    ) -> Result<String, ZkProverError> {
        let backend_session_id = Self::backend_session_id(request_session_id);
        let result = self.prove_range(request, request_session_id).await?;
        self.completed_results
            .lock()
            .map_err(|e| backend_error!("dry-run result store lock poisoned: {e}"))?
            .insert(backend_session_id.clone(), result);

        Ok(backend_session_id)
    }

    async fn submit_next(
        &self,
        _request: &SnarkPlonkProofRequest,
        request_session_id: &str,
        completed_backend_session_id: &str,
    ) -> Result<String, ZkProverError> {
        let backend_session_id =
            Self::backend_session_id_for_type(SessionType::Snark, request_session_id);
        Self::advance_range_to_snark(
            &self.completed_results,
            backend_session_id,
            completed_backend_session_id,
        )
    }

    async fn poll(&self, backend_session_id: &str) -> Result<ZkSessionState, ZkProverError> {
        let contains_result = self
            .completed_results
            .lock()
            .map_err(|e| backend_error!("dry-run result store lock poisoned: {e}"))?
            .contains_key(backend_session_id);
        if contains_result { Ok(ZkSessionState::Completed) } else { Ok(ZkSessionState::NotFound) }
    }

    async fn download(
        &self,
        _session_type: SessionType,
        backend_session_id: &str,
    ) -> Result<ProofResult, ZkProverError> {
        self.completed_results
            .lock()
            .map_err(|e| backend_error!("dry-run result store lock poisoned: {e}"))?
            .remove(backend_session_id)
            .ok_or_else(|| ZkProverError::BackendSessionNotFound {
                backend_session_id: backend_session_id.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use base_prover_service_protocol::{ProofResult, SnarkPlonkProofResult, ZkProofResult, ZkVm};

    use super::{DRY_RUN_SNARK_PREFIX, DRY_RUN_STARK_PREFIX, DryRunZkProver};

    fn stark_key(session: &str) -> String {
        format!("{DRY_RUN_STARK_PREFIX}{session}")
    }

    fn snark_key(session: &str) -> String {
        format!("{DRY_RUN_SNARK_PREFIX}{session}")
    }

    fn empty_proof() -> ZkProofResult {
        ZkProofResult { zk_vm: ZkVm::Sp1, proof: Vec::new().into(), execution_stats: None }
    }

    #[test]
    fn advance_range_to_snark_swaps_range_result_for_snark() {
        let session = "session-1";
        let store = Mutex::new(HashMap::from([(
            stark_key(session),
            ProofResult::Compressed(empty_proof()),
        )]));

        let snark_id =
            DryRunZkProver::advance_range_to_snark(&store, snark_key(session), &stark_key(session))
                .unwrap();

        assert_eq!(snark_id, snark_key(session));
        let guard = store.lock().unwrap();
        assert!(!guard.contains_key(&stark_key(session)), "range result should be consumed");
        assert!(matches!(guard.get(&snark_key(session)), Some(ProofResult::SnarkPlonk(_))));
    }

    #[test]
    fn advance_range_to_snark_is_idempotent_on_resume() {
        let session = "session-1";
        let store = Mutex::new(HashMap::from([(
            snark_key(session),
            ProofResult::SnarkPlonk(SnarkPlonkProofResult { proof: empty_proof() }),
        )]));

        let snark_id =
            DryRunZkProver::advance_range_to_snark(&store, snark_key(session), &stark_key(session))
                .unwrap();

        assert_eq!(snark_id, snark_key(session));
        assert_eq!(store.lock().unwrap().len(), 1, "store should be unchanged on resume");
    }
}
