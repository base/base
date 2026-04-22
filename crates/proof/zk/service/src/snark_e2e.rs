//! Shared SNARK Groth16 end-to-end test logic.
//!
//! Used by both the integration test (`tests/snark_groth16_e2e.rs`) and the
//! standalone binary (`bin/prover/zk/src/bin/snark_e2e.rs`) that runs as a K8s
//! `CronJob`.

use alloy_provider::{Identity, Provider, ProviderBuilder};
use alloy_rpc_types::{BlockId, BlockNumberOrTag};
use anyhow::{Context, Result, bail};
use base_common_network::Base;
use base_zk_client::{
    GetProofRequest, ProveBlockRequest, get_proof_response,
    prover_service_client::ProverServiceClient,
};
use sp1_sdk::{
    SP1ProofWithPublicValues, SP1VerifyingKey,
    blocking::{CpuProver, MockProver, Prover as BlockingProver},
};
use tonic::transport::Channel;
use tracing::{info, warn};

use crate::L1HeadCalculator;

const PROOF_TYPE_SNARK_GROTH16: i32 = 4;
const RECEIPT_TYPE_SNARK: i32 = 2; // ReceiptType::Snark

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
    async fn connect() -> Result<ProverServiceClient<Channel>> {
        let addr = std::env::var("PROVER_GRPC_ADDR")
            .unwrap_or_else(|_| "http://localhost:9090".to_string());

        info!(addr = %addr, "connecting to prover-service");
        let client = ProverServiceClient::connect(addr)
            .await
            .context("failed to connect to prover-service")?;
        Ok(client)
    }

    /// Verify the SNARK proof using the appropriate prover for the backend.
    ///
    /// - Mock backend: uses `MockProver::verify()` (checks public input hashes
    ///   only)
    /// - Cluster backend: uses `CpuProver::verify()` (full cryptographic
    ///   verification)
    async fn verify_snark_proof(
        snark_proof: SP1ProofWithPublicValues,
        agg_vk: SP1VerifyingKey,
        is_mock: bool,
    ) -> Result<()> {
        if is_mock {
            info!("verifying SNARK Groth16 proof with MockProver (BACKEND=mock)");
            let t = std::time::Instant::now();
            tokio::task::spawn_blocking(move || {
                let prover = MockProver::new();
                prover.verify(&snark_proof, &agg_vk, None).map_err(|e| {
                    anyhow::anyhow!("SNARK Groth16 mock proof verification failed: {e}")
                })
            })
            .await??;
            info!(
                elapsed_secs = t.elapsed().as_secs_f64(),
                "SNARK Groth16 proof verified (MockProver)"
            );
        } else {
            info!("verifying SNARK Groth16 proof with CpuProver (BACKEND=cluster)");
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
            info!(
                elapsed_secs = t.elapsed().as_secs_f64(),
                "SNARK Groth16 proof verified (CpuProver)"
            );
        }

        Ok(())
    }

    /// Run the full SNARK Groth16 E2E test pipeline:
    ///
    /// 1. Query the L2 node for the safe head block (guaranteed derived from
    ///    L1)
    /// 2. Submit a `ProveBlock` request with `proof_type=4` (SNARK Groth16)
    ///    - `l1_head` is omitted so the prover service calculates it via `SafeDB`
    /// 3. Poll `GetProof` with `receipt_type=SNARK` until completion or timeout
    /// 4. Deserialize the SNARK receipt
    /// 5. Compute the aggregation verifying key
    /// 6. Verify the SNARK proof (`MockProver` or `CpuProver` based on BACKEND)
    pub async fn run() -> Result<()> {
        let l2_rpc = std::env::var("L2_NODE_ADDRESS").context("L2_NODE_ADDRESS must be set")?;

        // -- 1. Query L2 safe head -----------------------------------------------
        //
        // Use the "safe" block tag instead of `latest - 1000`.  The safe head
        // is the highest L2 block that the node has derived from L1 data, so
        // its state and batch data are guaranteed to be available for witness
        // generation.  This avoids "Data source exhausted" failures that occur
        // when the target block's L1 batch hasn't been fully posted yet.
        let provider = ProviderBuilder::new().connect_http(l2_rpc.parse()?);
        let latest_block = provider.get_block_number().await?;
        let safe_block = provider
            .get_block_by_number(BlockNumberOrTag::Safe)
            .await?
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

        let l1_provider = ProviderBuilder::new().connect_http(l1_url.parse()?);
        let op_provider = ProviderBuilder::<Identity, Identity, Base>::default()
            .connect_http(l2_consensus_url.parse()?);

        let finalized_l1 = l1_provider
            .get_block(BlockId::Number(BlockNumberOrTag::Finalized))
            .await?
            .context("L1 finalized block not available")?
            .header
            .number;

        let mut attempts = 0u64;
        loop {
            let l1_origin = L1HeadCalculator::get_l1_origin_num(&op_provider, target_block).await?;

            if l1_origin + SEQUENCE_WINDOW <= finalized_l1 {
                info!(
                    target_block,
                    l1_origin,
                    finalized_l1,
                    buffer = finalized_l1 - l1_origin,
                    "L1 finalized check passed"
                );
                break;
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
        }

        // -- 2. Submit ProveBlock with proof_type=4 (SNARK Groth16) ---------------
        //
        // l1_head is omitted -- the prover service calculates it server-side
        // using SafeDB (optimism_safeHeadAtL1Block) with a sequence_window
        // fallback, which is more robust than the client-side l1_origin + 50
        // heuristic.
        let mut client = Self::connect().await?;
        let prove_resp = client
            .prove_block(ProveBlockRequest {
                start_block_number: safe_head,
                number_of_blocks_to_prove: 1,
                sequence_window: Some(SEQUENCE_WINDOW),
                proof_type: PROOF_TYPE_SNARK_GROTH16,
                session_id: None,
                prover_address: Some("0x0000000000000000000000000000000000000000".to_string()),
                l1_head: None,
            })
            .await?;

        let session_id = prove_resp.into_inner().session_id;
        info!(session_id = %session_id, "ProveBlock submitted");

        // -- 3. Poll GetProof until SUCCEEDED or timeout --------------------------
        let start = std::time::Instant::now();
        let snark_receipt_bytes = loop {
            if start.elapsed().as_secs() > POLL_TIMEOUT_SECS {
                bail!("timed out after {POLL_TIMEOUT_SECS}s waiting for SNARK proof to complete");
            }

            tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;

            let resp = client
                .get_proof(GetProofRequest {
                    session_id: session_id.clone(),
                    receipt_type: Some(RECEIPT_TYPE_SNARK),
                })
                .await?;

            let inner = resp.into_inner();
            let status = get_proof_response::Status::try_from(inner.status)
                .unwrap_or(get_proof_response::Status::Unspecified);

            info!(
                elapsed_secs = start.elapsed().as_secs(),
                status = ?status,
                "poll status"
            );

            match status {
                get_proof_response::Status::Succeeded => {
                    if inner.receipt.is_empty() {
                        bail!("SNARK receipt is empty on SUCCEEDED status");
                    }
                    break inner.receipt;
                }
                get_proof_response::Status::Failed => {
                    bail!("proof generation FAILED for session_id: {session_id}");
                }
                get_proof_response::Status::Created
                | get_proof_response::Status::Pending
                | get_proof_response::Status::Running
                | get_proof_response::Status::Unspecified => {
                    // Still in progress, continue polling
                }
            }
        };

        info!(
            elapsed_secs = start.elapsed().as_secs(),
            receipt_bytes = snark_receipt_bytes.len(),
            "SNARK proof completed"
        );

        // -- 4. Deserialize SNARK receipt -----------------------------------------
        let (snark_proof, _): (SP1ProofWithPublicValues, _) =
            bincode::serde::decode_from_slice(&snark_receipt_bytes, bincode::config::standard())?;

        info!("SNARK proof deserialized successfully");

        // -- 5. Compute aggregation verifying key ---------------------------------
        info!("computing aggregation verifying key (LightProver — VK only)");
        let t = std::time::Instant::now();
        let (_range_vk, agg_vk) = base_succinct_proof_utils::cluster_setup_vkeys()
            .await
            .context("failed to compute aggregation verifying key")?;
        info!(elapsed_secs = t.elapsed().as_secs_f64(), "aggregation verifying key computed");

        // -- 6. Verify SNARK proof ------------------------------------------------
        let is_mock = std::env::var("BACKEND").map(|v| v == "mock").unwrap_or(false);

        Self::verify_snark_proof(snark_proof, agg_vk, is_mock).await?;

        Ok(())
    }
}
