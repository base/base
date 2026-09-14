//! Shared SP1 dry-run prove helpers for system tests.

use std::time::Duration;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use alloy_provider::RootProvider;
use base_common_network::Base;
use base_optimism_rpc::OptimismRollupProviderExt;
use base_prover_service_client::{ProofRequesterClient, ProverServiceClientConfig};
use base_prover_service_protocol::{
    ExecutionStats, GetProofRequest, GetProofResponse, ProofRequest, ProofRequestKind, ProofResult,
    ProofStatus, ProveBlockRangeRequest, ZkBackend, ZkProofRequest, ZkVm,
};
use eyre::{Result, WrapErr};
use nanoid::nanoid;
use tokio::time::{sleep, timeout};
use url::Url;

const SAFE_L2_TIMEOUT: Duration = Duration::from_secs(120);
const SAFE_L2_POLL_INTERVAL: Duration = Duration::from_millis(500);
const PROOF_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const PROOF_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Waits until `block_number` is safe and has an output root.
pub(crate) async fn wait_for_safe_l2(
    provider: &RootProvider<Base>,
    block_number: u64,
) -> Result<()> {
    match timeout(SAFE_L2_TIMEOUT, async {
        loop {
            let status = provider.optimism_sync_status().await?;
            if status.safe_l2.number >= block_number {
                provider.optimism_output_at_block(BlockNumberOrTag::Number(block_number)).await?;
                return Ok::<_, eyre::Error>(());
            }
            sleep(SAFE_L2_POLL_INTERVAL).await;
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let status = provider.optimism_sync_status().await?;
            eyre::bail!(
                "timed out waiting for block {block_number} to become safe \
                 (safe_l2={}, unsafe_l2={})",
                status.safe_l2.number,
                status.unsafe_l2.number
            );
        }
    }
}

/// Dry-run proves a one-block range ending at `block_number`.
pub(crate) async fn prove_block_range_with_dry_run_stats(
    prover_url: Url,
    block_number: u64,
    l1_head: B256,
    session_prefix: &str,
) -> Result<ExecutionStats> {
    let start_block_number = block_number
        .checked_sub(1)
        .ok_or_else(|| eyre::eyre!("cannot prove genesis block with one-block range"))?;
    let client_config = ProverServiceClientConfig::new(prover_url.as_str())
        .with_request_timeout(Duration::from_secs(30));
    let client = ProofRequesterClient::connect(&client_config)?;
    let session_id = format!("{session_prefix}-{}", nanoid!());
    let response = client
        .prove_block_range(ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id,
                request: ProofRequestKind::Compressed(ZkProofRequest {
                    start_block_number,
                    number_of_blocks_to_prove: 1,
                    sequence_window: None,
                    l1_head: Some(l1_head),
                    intermediate_root_interval: None,
                    schedule_l2_block_number: None,
                    zk_vm: ZkVm::Sp1,
                    zk_backend: ZkBackend::DryRun,
                }),
            },
            retry_failed: true,
        })
        .await?;

    poll_dry_run_stats(&client, response.session_id).await
}

async fn poll_dry_run_stats(
    client: &ProofRequesterClient,
    session_id: String,
) -> Result<ExecutionStats> {
    let timeout_session_id = session_id.clone();
    match timeout(PROOF_TIMEOUT, async {
        loop {
            let response =
                client.get_proof(GetProofRequest { session_id: session_id.clone() }).await?;
            match response.status {
                ProofStatus::Succeeded => {
                    return execution_stats_from_response(&session_id, response);
                }
                ProofStatus::Failed => {
                    return Err(eyre::eyre!(
                        "proof request failed: {}",
                        response
                            .error_message
                            .unwrap_or_else(|| "missing error message".to_string())
                    ));
                }
                _ => sleep(PROOF_POLL_INTERVAL).await,
            }
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let last = client
                .get_proof(GetProofRequest { session_id: timeout_session_id.clone() })
                .await
                .wrap_err_with(|| {
                    format!(
                        "timed out waiting for proof request {timeout_session_id}; \
                         also failed to fetch last status"
                    )
                })?;
            eyre::bail!(
                "timed out waiting for proof request {timeout_session_id} \
                 (status={:?}, error={})",
                last.status,
                last.error_message.unwrap_or_else(|| "none".to_string())
            );
        }
    }
}

fn execution_stats_from_response(
    session_id: &str,
    response: GetProofResponse,
) -> Result<ExecutionStats> {
    match response.result {
        Some(ProofResult::Compressed(result)) => result.execution_stats.ok_or_else(|| {
            eyre::eyre!(
                "dry-run prover response for request {session_id} did not include execution_stats"
            )
        }),
        Some(ProofResult::SnarkPlonk(_)) => Err(eyre::eyre!(
            "dry-run prover response for request {session_id} returned snark_plonk result"
        )),
        Some(ProofResult::Tee(_)) => {
            Err(eyre::eyre!("dry-run prover response for request {session_id} returned tee result"))
        }
        None => Err(eyre::eyre!(
            "dry-run prover response for request {session_id} did not include a result"
        )),
    }
}
