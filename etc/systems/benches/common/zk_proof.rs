//! ZK proof request helpers for system benchmarks.

use std::time::{Duration, Instant};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use alloy_provider::RootProvider;
use base_common_network::Base;
use base_optimism_rpc::OptimismRollupProviderExt;
use base_zk_client::{
    ExecutionStats, GetProofRequest, ProofJobStatus, ProofType, ProveBlockRequest, ReceiptType,
    ZkProofClient, ZkProofClientConfig,
};
use eyre::{Result, WrapErr, ensure};
use tokio::time::{sleep, timeout};
use url::Url;

use super::BenchDisplay;

/// ZK proof helpers for system benchmarks.
#[derive(Debug)]
pub struct ZkProofBench;

/// Timing configuration for waiting on safe L2 blocks and ZK proof jobs.
#[derive(Clone, Copy, Debug)]
pub struct ZkProofBenchConfig {
    /// Timeout for the workload block to become safe.
    pub safe_l2_timeout: Duration,
    /// Polling interval while waiting for safe L2.
    pub safe_l2_poll_interval: Duration,
    /// Timeout for the dry-run proof request.
    pub proof_timeout: Duration,
    /// Polling interval while waiting for proof completion.
    pub proof_poll_interval: Duration,
}

impl ZkProofBench {
    /// Waits for a block range to become safe, then requests dry-run proof stats for it.
    pub async fn prove_safe_block_range_with_dry_run_stats(
        rollup_provider: &RootProvider<Base>,
        prover_url: Url,
        first_block_number: u64,
        last_block_number: u64,
        config: ZkProofBenchConfig,
        display: &BenchDisplay,
    ) -> Result<ExecutionStats> {
        let l1_head = Self::wait_for_safe_l2(
            rollup_provider,
            last_block_number,
            config.safe_l2_timeout,
            config.safe_l2_poll_interval,
            display,
        )
        .await?;

        Self::prove_block_range_with_dry_run_stats(
            prover_url,
            first_block_number,
            last_block_number,
            l1_head,
            config.proof_timeout,
            config.proof_poll_interval,
            display,
        )
        .await
    }

    /// Waits for a workload block to become safe and returns the current L1 head.
    pub async fn wait_for_safe_l2(
        provider: &RootProvider<Base>,
        block_number: u64,
        wait_timeout: Duration,
        poll_interval: Duration,
        display: &BenchDisplay,
    ) -> Result<B256> {
        timeout(wait_timeout, async {
            loop {
                let status = provider.optimism_sync_status().await?;
                display.safe_l2_progress(status.safe_l2.number, block_number);
                if status.safe_l2.number >= block_number {
                    provider
                        .optimism_output_at_block(BlockNumberOrTag::Number(block_number))
                        .await?;
                    display.safe_l2_done(block_number);
                    return Ok::<_, eyre::Error>(status.head_l1.hash);
                }
                sleep(poll_interval).await;
            }
        })
        .await
        .wrap_err("timed out waiting for workload block to become safe")?
    }

    /// Requests a dry-run proof for a block range and returns execution stats.
    pub async fn prove_block_range_with_dry_run_stats(
        prover_url: Url,
        first_block_number: u64,
        last_block_number: u64,
        l1_head: B256,
        proof_timeout: Duration,
        poll_interval: Duration,
        display: &BenchDisplay,
    ) -> Result<ExecutionStats> {
        ensure!(
            last_block_number >= first_block_number,
            "invalid workload block range: {first_block_number}..={last_block_number}"
        );
        let start_block_number = first_block_number
            .checked_sub(1)
            .ok_or_else(|| eyre::eyre!("cannot prove genesis block with one-block range"))?;
        let number_of_blocks_to_prove = last_block_number - first_block_number + 1;
        let client = ZkProofClient::new(&ZkProofClientConfig {
            endpoint: prover_url,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
        })?;
        let response = client
            .prove_block(ProveBlockRequest {
                start_block_number,
                number_of_blocks_to_prove,
                sequence_window: None,
                proof_type: ProofType::Compressed.into(),
                session_id: None,
                prover_address: None,
                l1_head: Some(l1_head.to_string()),
                intermediate_root_interval: None,
            })
            .await?;

        display.proof_requested(
            &response.session_id,
            start_block_number,
            number_of_blocks_to_prove,
        );
        Self::poll_dry_run_stats(
            &client,
            response.session_id,
            proof_timeout,
            poll_interval,
            display,
        )
        .await
    }

    /// Polls a dry-run proof job until it returns execution stats or times out.
    pub async fn poll_dry_run_stats(
        client: &ZkProofClient,
        session_id: String,
        proof_timeout: Duration,
        poll_interval: Duration,
        display: &BenchDisplay,
    ) -> Result<ExecutionStats> {
        let timeout_session_id = session_id.clone();
        timeout(proof_timeout, async {
            let start = Instant::now();
            loop {
                let response = client
                    .get_proof(GetProofRequest {
                        session_id: session_id.clone(),
                        receipt_type: Some(ReceiptType::Stark.into()),
                    })
                    .await?;
                let status = ProofJobStatus::try_from(response.status)
                    .unwrap_or(ProofJobStatus::Unspecified);
                display.proof_progress(&status, start.elapsed());

                match status {
                    ProofJobStatus::Succeeded => {
                        return response.execution_stats.ok_or_else(|| {
                            eyre::eyre!("dry-run prover response did not include execution_stats")
                        });
                    }
                    ProofJobStatus::Failed => {
                        return Err(eyre::eyre!(
                            "proof request failed: {}",
                            response
                                .error_message
                                .unwrap_or_else(|| "missing error message".to_string())
                        ));
                    }
                    _ => sleep(poll_interval).await,
                }
            }
        })
        .await
        .wrap_err_with(|| format!("timed out waiting for proof request {timeout_session_id}"))?
    }
}
