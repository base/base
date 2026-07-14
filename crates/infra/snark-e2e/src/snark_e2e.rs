//! Shared SNARK Groth16 end-to-end test logic.
//!
//! Used by both the integration test (`tests/snark_groth16_e2e.rs`) and the
//! standalone binary (`bin/snark-e2e`) that runs as a K8s `CronJob`.
//!
//! Talks to the JSON-RPC prover-service requester API (`prover_proveBlockRange`
//! / `prover_getProof`), not the legacy zk gRPC service.

use alloy_primitives::Address;
use alloy_provider::{Identity, Provider, ProviderBuilder};
use alloy_rpc_types::{BlockId, BlockNumberOrTag};
use anyhow::{Context, Result, bail};
use base_common_network::Base;
use base_l1_head::L1HeadCalculator;
use base_prover_service_client::{ProofRequesterClient, ProverServiceClientConfig};
use base_prover_service_protocol::{
    GetProofRequest, ProofRequest, ProofRequestKind, ProofResult, ProofStatus,
    ProveBlockRangeRequest, SnarkGroth16ProofRequest, ZkBackend, ZkProofRequest, ZkVm,
};
use sp1_sdk::{
    SP1ProofWithPublicValues, SP1VerifyingKey,
    blocking::{CpuProver, Prover as BlockingProver},
};
use tracing::{info, warn};
use uuid::Uuid;

const POLL_INTERVAL_SECS: u64 = 30;
const POLL_TIMEOUT_SECS: u64 = 14400; // 4 hours

/// Number of L1 blocks past the L1 origin to include when computing the
/// `l1_head` for witness generation.  The server-side fallback computes
/// `l1_head = min(l1_origin + sequence_window, finalized_l1)`.
const SEQUENCE_WINDOW: u64 = 100;

/// When L1 hasn't finalized far enough for the target L2 block, step back
/// this many L2 blocks at a time and recheck.
const L2_BLOCK_STEP_BACK: u64 = 10;

/// Maximum number of times we step back before giving up.
const MAX_STEP_BACKS: u64 = 300;

/// SNARK Groth16 end-to-end test runner.
#[derive(Debug)]
pub struct SnarkE2e;

impl SnarkE2e {
    fn connect() -> Result<ProofRequesterClient> {
        let addr = std::env::var("PROVER_RPC_ADDR")
            .unwrap_or_else(|_| "http://localhost:9000".to_string());

        info!(addr = %addr, "connecting to prover-service");
        let config = ProverServiceClientConfig::new(&addr);
        ProofRequesterClient::connect(&config).context("failed to connect to prover-service")
    }

    /// Verify the SNARK proof with `CpuProver` (full cryptographic verification).
    async fn verify_snark_proof(
        snark_proof: SP1ProofWithPublicValues,
        agg_vk: SP1VerifyingKey,
    ) -> Result<()> {
        info!("verifying SNARK Groth16 proof with CpuProver");
        let t = std::time::Instant::now();
        tokio::task::spawn_blocking(move || {
            info!("creating CpuProver");
            let prover = CpuProver::new();
            info!("CpuProver created, running verify");
            prover
                .verify(&snark_proof, &agg_vk, None)
                .map_err(|e| anyhow::anyhow!("SNARK Groth16 proof verification failed: {e}"))
        })
        .await??;
        info!(elapsed_secs = t.elapsed().as_secs_f64(), "SNARK Groth16 proof verified (CpuProver)");

        Ok(())
    }

    /// Extract SNARK receipt bytes from a successful getProof response.
    fn snark_receipt_bytes(result: ProofResult) -> Result<Vec<u8>> {
        match result {
            ProofResult::SnarkGroth16(r) => Ok(r.proof.proof.to_vec()),
            ProofResult::Compressed(_) => {
                bail!("expected SnarkGroth16 proof result, got Compressed")
            }
            ProofResult::Tee(_) => bail!("expected SnarkGroth16 proof result, got Tee"),
        }
    }

    /// Run the full SNARK Groth16 E2E test pipeline:
    ///
    /// 1. Query the L2 node for the safe head block (guaranteed derived from
    ///    L1)
    /// 2. Submit a `prover_proveBlockRange` request for `SnarkGroth16`
    ///    - `l1_head` is omitted so the prover service calculates it via `SafeDB`
    /// 3. Poll `prover_getProof` until completion or timeout
    /// 4. Deserialize the SNARK receipt
    /// 5. Compute the aggregation verifying key
    /// 6. Verify the SNARK proof with `CpuProver`
    pub async fn run() -> Result<()> {
        let l2_rpc = std::env::var("L2_NODE_ADDRESS").context("L2_NODE_ADDRESS must be set")?;

        // -- 1. Query L2 safe head -----------------------------------------------
        //
        // Use the "safe" block tag instead of `latest - 1000`.  The safe head
        // is the highest L2 block that the node has derived from L1 data, so
        // its state and batch data are guaranteed to be available for witness
        // generation.  This avoids "Data source exhausted" failures that occur
        // when the target block's L1 batch hasn't been fully posted yet.
        let provider = ProviderBuilder::new()
            .connect_http(l2_rpc.parse().context("invalid L2_NODE_ADDRESS URL")?);
        let latest_block =
            provider.get_block_number().await.context("failed to fetch latest L2 block number")?;
        let safe_block = provider
            .get_block_by_number(BlockNumberOrTag::Safe)
            .await
            .context("failed to fetch L2 safe block")?
            .context("L2 safe block not available")?;
        let safe_head_number = safe_block.header.number;

        // Prove 1 block: start_block_number = safe_head - 1, target = safe_head
        let mut target_block = safe_head_number;
        let mut safe_head = target_block - 1;
        info!(
            latest_block,
            safe_head_number, target_block, safe_head, "fetched L2 block numbers (using safe head)"
        );

        // -- 1b. Ensure L1 has finalized far enough ------------------------------
        //
        // The server computes l1_head = min(l1_origin + sequence_window,
        // finalized_l1). When SafeDB is unavailable this is the only path, and
        // if finalized_l1 is too low the effective buffer gets truncated, causing
        // "Data source exhausted" during witness generation.
        //
        // Pre-flight check: verify that l1_origin + SEQUENCE_WINDOW <=
        // finalized_l1 for our target block.  If not, step back to an older L2
        // block where the condition holds.
        let l1_url = std::env::var("L1_NODE_ADDRESS").context("L1_NODE_ADDRESS must be set")?;
        let l2_consensus_url = std::env::var("BASE_CONSENSUS_ADDRESS")
            .context("BASE_CONSENSUS_ADDRESS must be set")?;

        let l1_provider = ProviderBuilder::new()
            .connect_http(l1_url.parse().context("invalid L1_NODE_ADDRESS URL")?);
        let op_provider = ProviderBuilder::<Identity, Identity, Base>::default()
            .connect_http(l2_consensus_url.parse().context("invalid BASE_CONSENSUS_ADDRESS URL")?);

        let finalized_l1 = l1_provider
            .get_block(BlockId::Number(BlockNumberOrTag::Finalized))
            .await
            .context("failed to fetch finalized L1 block")?
            .context("L1 finalized block not available")?
            .header
            .number;

        let mut attempts = 0u64;
        let selected_l1_origin = loop {
            let l1_origin = L1HeadCalculator::get_l1_origin_num(&op_provider, target_block)
                .await
                .with_context(|| {
                format!("failed to fetch L1 origin for target L2 block {target_block}")
            })?;

            if l1_origin + SEQUENCE_WINDOW <= finalized_l1 {
                info!(
                    target_block,
                    safe_head,
                    l1_origin,
                    finalized_l1,
                    buffer = finalized_l1 - l1_origin,
                    step_back_attempts = attempts,
                    "L1 finalized check passed"
                );
                break l1_origin;
            }

            attempts += 1;
            if attempts > MAX_STEP_BACKS {
                bail!(
                    "L1 finalized block ({finalized_l1}) is too low for target L2 block \
                     {target_block} (l1_origin={l1_origin}, need l1_origin+{SEQUENCE_WINDOW}={}). \
                     Try again later or enable SafeDB on the op-node.",
                    l1_origin + SEQUENCE_WINDOW
                );
            }

            warn!(
                target_block,
                l1_origin,
                finalized_l1,
                needed = l1_origin + SEQUENCE_WINDOW,
                gap = (l1_origin + SEQUENCE_WINDOW) as i64 - finalized_l1 as i64,
                "L1 not finalized far enough, stepping back L2 blocks"
            );
            target_block -= L2_BLOCK_STEP_BACK;
            safe_head = target_block - 1;
        };

        // -- 2. Submit proveBlockRange (SnarkGroth16) -----------------------------
        //
        // l1_head is omitted -- the prover service calculates it server-side
        // using SafeDB (optimism_safeHeadAtL1Block) with a sequence_window
        // fallback, which is more robust than the client-side l1_origin + 50
        // heuristic.
        let client = Self::connect()?;
        let session_id = Uuid::new_v4().to_string();
        let prove_resp = client
            .prove_block_range(ProveBlockRangeRequest {
                proof: ProofRequest {
                    session_id: session_id.clone(),
                    request: ProofRequestKind::SnarkGroth16(SnarkGroth16ProofRequest {
                        proof: ZkProofRequest {
                            start_block_number: safe_head,
                            number_of_blocks_to_prove: 1,
                            sequence_window: Some(SEQUENCE_WINDOW),
                            l1_head: None,
                            intermediate_root_interval: None,
                            zk_vm: ZkVm::Sp1,
                            zk_backend: ZkBackend::Cluster,
                        },
                        prover_address: Address::ZERO,
                    }),
                },
            })
            .await
            .with_context(|| {
                format!(
                    "failed to submit proveBlockRange for start_block={safe_head}, \
                     target_block={target_block}"
                )
            })?;

        let session_id = prove_resp.session_id;
        info!(
            session_id = %session_id,
            start_block = safe_head,
            target_block,
            selected_l1_origin,
            finalized_l1,
            sequence_window = SEQUENCE_WINDOW,
            step_back_attempts = attempts,
            "proveBlockRange submitted"
        );

        // -- 3. Poll getProof until Succeeded or timeout --------------------------
        let start = std::time::Instant::now();
        let snark_receipt_bytes = loop {
            if start.elapsed().as_secs() > POLL_TIMEOUT_SECS {
                bail!(
                    "timed out after {POLL_TIMEOUT_SECS}s waiting for SNARK proof to complete: \
                     session_id={session_id}, start_block={safe_head}, target_block={target_block}"
                );
            }

            tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;

            let resp = client
                .get_proof(GetProofRequest { session_id: session_id.clone() })
                .await
                .with_context(|| {
                format!("failed to poll getProof for session_id={session_id}")
            })?;

            let error_message = resp.error_message.as_deref().unwrap_or("");
            let receipt_bytes = resp.result.as_ref().map(|r| match r {
                ProofResult::SnarkGroth16(r) => r.proof.proof.len(),
                ProofResult::Compressed(r) => r.proof.len(),
                ProofResult::Tee(_) => 0,
            });

            info!(
                session_id = %session_id,
                start_block = safe_head,
                target_block,
                elapsed_secs = start.elapsed().as_secs(),
                status = ?resp.status,
                receipt_bytes,
                error_message,
                "poll status"
            );

            match resp.status {
                ProofStatus::Succeeded => {
                    let result = resp.result.with_context(|| {
                        format!(
                            "SNARK result missing on Succeeded status: \
                             session_id={session_id}, start_block={safe_head}, \
                             target_block={target_block}"
                        )
                    })?;
                    let bytes = Self::snark_receipt_bytes(result)?;
                    if bytes.is_empty() {
                        bail!(
                            "SNARK receipt is empty on Succeeded status: \
                             session_id={session_id}, start_block={safe_head}, \
                             target_block={target_block}"
                        );
                    }
                    break bytes;
                }
                ProofStatus::Failed => {
                    let prover_error = if error_message.is_empty() {
                        "no error_message returned by prover"
                    } else {
                        error_message
                    };
                    bail!(
                        "proof generation FAILED: session_id={session_id}, \
                         start_block={safe_head}, target_block={target_block}, \
                         elapsed_secs={}, prover_error={prover_error}",
                        start.elapsed().as_secs()
                    );
                }
                ProofStatus::Queued | ProofStatus::Running => {
                    // Still in progress, continue polling
                }
            }
        };

        info!(
            session_id = %session_id,
            start_block = safe_head,
            target_block,
            elapsed_secs = start.elapsed().as_secs(),
            receipt_bytes = snark_receipt_bytes.len(),
            "SNARK proof completed"
        );

        // -- 4. Deserialize SNARK receipt -----------------------------------------
        let (snark_proof, _): (SP1ProofWithPublicValues, _) =
            bincode::serde::decode_from_slice(&snark_receipt_bytes, bincode::config::standard())
                .with_context(|| {
                    format!(
                        "failed to deserialize SNARK receipt for session_id={session_id}, \
                         receipt_bytes={}",
                        snark_receipt_bytes.len()
                    )
                })?;

        info!("SNARK proof deserialized successfully");

        // -- 5. Compute aggregation verifying key ---------------------------------
        info!("computing aggregation verifying key (LightProver — VK only)");
        let t = std::time::Instant::now();
        let (_range_vk, agg_vk) = base_proof_succinct_proof_utils::cluster_setup_vkeys()
            .await
            .context("failed to compute aggregation verifying key")?;
        info!(elapsed_secs = t.elapsed().as_secs_f64(), "aggregation verifying key computed");

        // -- 6. Verify SNARK proof ------------------------------------------------
        Self::verify_snark_proof(snark_proof, agg_vk)
            .await
            .with_context(|| format!("failed to verify SNARK proof for session_id={session_id}"))?;

        Ok(())
    }
}
