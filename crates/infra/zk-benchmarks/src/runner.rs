//! ZK proof benchmark runner for completed load-test runs.

use std::time::{Duration, Instant};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_load_tests::{MetricsSummary, QueryProvider, RpcProviders};
use base_optimism_rpc::OptimismRollupProviderExt;
use base_prover_service_client::{ProofRequesterClient, ProverServiceClientConfig};
use base_prover_service_protocol::{
    ExecutionStats, GetProofRequest, ProofRequest, ProofRequestKind, ProofResult, ProofStatus,
    ProveBlockRangeRequest, ZkBackend, ZkProofRequest, ZkVm,
};
use eyre::{Result, WrapErr, ensure};
use nanoid::nanoid;
use tokio::time::{sleep, timeout};

use crate::types::{ZkBenchConfig, ZkBenchProofOutcome, ZkBenchSummary, ZkBenchTarget};

const SAFE_L2_TIMEOUT: Duration = Duration::from_secs(300);
const SAFE_L2_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Runs ZK proof benchmarks for completed load-test summaries.
#[derive(Debug)]
pub struct ZkBenchRunner;

impl ZkBenchRunner {
    /// Selects the proof target, waits for safe L2, requests a proof, and polls for completion.
    pub async fn run(summary: &MetricsSummary, config: ZkBenchConfig) -> Result<ZkBenchSummary> {
        if let Some(error) = &summary.error {
            return Err(
                eyre::eyre!("{error}").wrap_err("cannot run ZK bench: load test reported an error")
            );
        }

        let target = Self::select_proof_target(summary)?;
        let rollup_provider = RpcProviders::query(config.rollup_rpc_url.clone())
            .map_err(eyre::Report::from)
            .wrap_err_with(|| format!("failed to connect rollup RPC {}", config.rollup_rpc_url))?;
        let (proof, execution_stats) =
            Self::prove_safe_block(&rollup_provider, target.block, config).await?;

        Ok(ZkBenchSummary { target, proof, execution_stats })
    }

    fn select_proof_target(summary: &MetricsSummary) -> Result<ZkBenchTarget> {
        ensure!(
            summary.receipt_coverage.is_complete(),
            "cannot select fullest block with incomplete receipt coverage"
        );
        let block = summary
            .fullest_block
            .as_ref()
            .ok_or_else(|| eyre::eyre!("cannot select fullest block from empty load-test run"))?;
        Ok(ZkBenchTarget {
            block: block.block_number,
            reason: format!(
                "fullest block by gas: gas={}, txs={}",
                block.total_gas, block.confirmed_count
            ),
        })
    }

    async fn prove_safe_block(
        rollup_provider: &QueryProvider,
        block_number: u64,
        config: ZkBenchConfig,
    ) -> Result<(ZkBenchProofOutcome, Option<ExecutionStats>)> {
        let l1_head = Self::wait_for_safe_l2(
            rollup_provider,
            block_number,
            SAFE_L2_TIMEOUT,
            SAFE_L2_POLL_INTERVAL,
        )
        .await?;

        Self::prove_block(block_number, l1_head, config).await
    }

    async fn wait_for_safe_l2(
        provider: &QueryProvider,
        block_number: u64,
        wait_timeout: Duration,
        poll_interval: Duration,
    ) -> Result<B256> {
        timeout(wait_timeout, async {
            loop {
                let status = provider.optimism_sync_status().await?;
                if status.safe_l2.number >= block_number {
                    // Preflight: output root must exist before proving.
                    let _ = provider
                        .optimism_output_at_block(BlockNumberOrTag::Number(block_number))
                        .await?;
                    return Ok::<_, eyre::Error>(status.head_l1.hash);
                }
                sleep(poll_interval).await;
            }
        })
        .await
        .wrap_err("timed out waiting for workload block to become safe")?
    }

    async fn prove_block(
        block_number: u64,
        l1_head: B256,
        config: ZkBenchConfig,
    ) -> Result<(ZkBenchProofOutcome, Option<ExecutionStats>)> {
        let start_block_number =
            block_number.checked_sub(1).ok_or_else(|| eyre::eyre!("cannot prove genesis block"))?;
        let zk_backend = config.zk_backend;
        let zk_artifact_hash = config.zk_artifact_hash;
        let client_config = ProverServiceClientConfig::new(config.prover_url.as_str());
        let proof_timeout = client_config.max_wait();
        let poll_interval = client_config.poll_interval();
        let client = ProofRequesterClient::connect(&client_config)
            .wrap_err_with(|| format!("failed to connect prover service {}", config.prover_url))?;
        let session_id = format!("zk-benchmarks-{}-{}", zk_backend.as_str(), nanoid!());
        let request = Self::proof_request(
            session_id,
            start_block_number,
            l1_head,
            zk_backend,
            zk_artifact_hash,
        );
        let proof_started = Instant::now();
        let response =
            client.prove_block_range(request).await.wrap_err("prove-block-range request failed")?;

        let execution_stats = Self::poll_proof(
            &client,
            &response.session_id,
            zk_backend,
            proof_timeout,
            poll_interval,
        )
        .await?;
        let outcome = ZkBenchProofOutcome {
            zk_backend,
            session_id: response.session_id,
            start_block_number,
            l1_head,
            proof_duration: proof_started.elapsed(),
        };

        Ok((outcome, execution_stats))
    }

    #[allow(clippy::missing_const_for_fn)] // heap types; not meant for const eval
    fn proof_request(
        session_id: String,
        start_block_number: u64,
        l1_head: B256,
        zk_backend: ZkBackend,
        zk_artifact_hash: B256,
    ) -> ProveBlockRangeRequest {
        ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id,
                request: ProofRequestKind::Compressed(ZkProofRequest {
                    start_block_number,
                    number_of_blocks_to_prove: 1,
                    sequence_window: None,
                    l1_head: Some(l1_head),
                    intermediate_root_interval: None,
                    schedule_l2_block_number: None,
                    zk_artifact_hash: Some(zk_artifact_hash),
                    zk_vm: ZkVm::Sp1,
                    zk_backend,
                }),
            },
            retry_failed: true,
        }
    }

    async fn poll_proof(
        client: &ProofRequesterClient,
        session_id: &str,
        zk_backend: ZkBackend,
        proof_timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Option<ExecutionStats>> {
        let get_proof_request = GetProofRequest { session_id: session_id.to_owned() };
        timeout(proof_timeout, async {
            let start = Instant::now();
            loop {
                let response = client
                    .get_proof(get_proof_request.clone())
                    .await
                    .wrap_err("get-proof request failed")?;

                match response.status {
                    ProofStatus::Succeeded => {
                        return Self::proof_result(zk_backend, session_id, response.result);
                    }
                    ProofStatus::Failed => {
                        return Err(eyre::eyre!(
                            "proof request failed after {:?}: {}",
                            start.elapsed(),
                            response
                                .error_message
                                .unwrap_or_else(|| "missing error message".to_string())
                        ));
                    }
                    ProofStatus::Queued | ProofStatus::Running => sleep(poll_interval).await,
                }
            }
        })
        .await
        .wrap_err_with(|| format!("timed out waiting for proof request {session_id}"))?
    }

    fn proof_result(
        zk_backend: ZkBackend,
        session_id: &str,
        result: Option<ProofResult>,
    ) -> Result<Option<ExecutionStats>> {
        let compressed = match result {
            Some(ProofResult::Compressed(compressed)) => compressed,
            Some(ProofResult::SnarkPlonk(_)) => {
                eyre::bail!("proof request {session_id} returned snark_plonk result")
            }
            Some(ProofResult::Tee(_)) => {
                eyre::bail!("proof request {session_id} returned tee result")
            }
            None => {
                eyre::bail!("proof request {session_id} succeeded without a result")
            }
        };

        match zk_backend {
            ZkBackend::DryRun => compressed.execution_stats.map(Some).ok_or_else(|| {
                eyre::eyre!("dry-run proof request {session_id} returned no execution stats")
            }),
            ZkBackend::Cluster | ZkBackend::Network => Ok(compressed.execution_stats),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_primitives::B256;
    use base_load_tests::{BlockLoadMetrics, MetricsSummary, ReceiptCoverage};
    use base_prover_service_protocol::{
        ExecutionStats, ProofRequestKind, ProofResult, SnarkPlonkProofResult, ZkBackend,
        ZkProofResult, ZkVm,
    };

    use super::*;

    #[test]
    fn select_proof_target_picks_fullest_block_by_gas() {
        let summary = summary(Some(block_load(11, 5, 2_000)));

        let target = ZkBenchRunner::select_proof_target(&summary).unwrap();

        assert_eq!(target.block, 11);
        assert_eq!(target.reason, "fullest block by gas: gas=2000, txs=5");
    }

    #[test]
    fn select_proof_target_rejects_incomplete_receipt_coverage() {
        let mut summary = summary(Some(block_load(11, 5, 2_000)));
        summary.receipt_coverage =
            ReceiptCoverage { blocks_total: 1, blocks_failed: 1, ..ReceiptCoverage::default() };

        let error = ZkBenchRunner::select_proof_target(&summary).unwrap_err();
        assert!(error.to_string().contains("incomplete receipt coverage"));
    }

    #[test]
    fn select_proof_target_rejects_missing_fullest_block() {
        let error = ZkBenchRunner::select_proof_target(&summary(None)).unwrap_err();
        assert!(error.to_string().contains("empty load-test run"));
    }

    #[test]
    fn proof_request_uses_parent_block_single_block_and_selected_backend() {
        let request = ZkBenchRunner::proof_request(
            "session".to_string(),
            9,
            B256::repeat_byte(0xaa),
            ZkBackend::Cluster,
            B256::repeat_byte(0xbb),
        );
        let ProofRequestKind::Compressed(proof) = request.proof.request else {
            panic!("expected compressed proof request");
        };

        assert_eq!(proof.start_block_number, 9);
        assert_eq!(proof.number_of_blocks_to_prove, 1);
        assert_eq!(proof.l1_head, Some(B256::repeat_byte(0xaa)));
        assert_eq!(proof.zk_artifact_hash, Some(B256::repeat_byte(0xbb)));
        assert_eq!(proof.zk_backend, ZkBackend::Cluster);
    }

    #[test]
    fn dry_run_requires_and_returns_execution_stats() {
        let error = ZkBenchRunner::proof_result(
            ZkBackend::DryRun,
            "session",
            Some(compressed_result(None)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("returned no execution stats"));

        let stats = execution_stats();
        let result = ZkBenchRunner::proof_result(
            ZkBackend::DryRun,
            "session",
            Some(compressed_result(Some(stats.clone()))),
        )
        .unwrap();
        assert_eq!(result, Some(stats));
    }

    #[test]
    fn proving_backends_accept_compressed_result_without_execution_stats() {
        for zk_backend in [ZkBackend::Cluster, ZkBackend::Network] {
            assert_eq!(
                ZkBenchRunner::proof_result(zk_backend, "session", Some(compressed_result(None)))
                    .unwrap(),
                None
            );
        }
        assert!(
            ZkBenchRunner::proof_result(ZkBackend::Cluster, "session", None)
                .unwrap_err()
                .to_string()
                .contains("succeeded without a result")
        );
    }

    #[test]
    fn successful_request_rejects_wrong_result_type() {
        let result = ProofResult::SnarkPlonk(SnarkPlonkProofResult { proof: zk_result(None) });

        let error =
            ZkBenchRunner::proof_result(ZkBackend::Cluster, "session", Some(result)).unwrap_err();

        assert!(error.to_string().contains("returned snark_plonk result"));
    }

    fn summary(fullest_block: Option<BlockLoadMetrics>) -> MetricsSummary {
        MetricsSummary { fullest_block, ..MetricsSummary::default() }
    }

    const fn block_load(
        block_number: u64,
        confirmed_count: u64,
        total_gas: u64,
    ) -> BlockLoadMetrics {
        BlockLoadMetrics { block_number, confirmed_count, total_gas }
    }

    fn compressed_result(execution_stats: Option<ExecutionStats>) -> ProofResult {
        ProofResult::Compressed(zk_result(execution_stats))
    }

    fn zk_result(execution_stats: Option<ExecutionStats>) -> ZkProofResult {
        ZkProofResult { zk_vm: ZkVm::Sp1, proof: Default::default(), execution_stats }
    }

    fn execution_stats() -> ExecutionStats {
        ExecutionStats {
            total_instruction_cycles: 10,
            total_sp1_gas: 20,
            cycle_tracker: HashMap::new(),
            witness_generation_ms: 30,
            execution_ms: 40,
        }
    }
}
