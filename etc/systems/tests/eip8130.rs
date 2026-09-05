//! System tests for EIP-8130 (type `0x79`) inclusion and dry-run proving.

#[path = "common/balance.rs"]
mod balance;
mod common;
#[path = "common/zenith.rs"]
mod zenith;

use std::time::Duration;

use alloy_consensus::Typed2718;
use alloy_eips::{BlockNumberOrTag, eip2718::Encodable2718};
use alloy_network::ReceiptResponse;
use alloy_primitives::{B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::{Eip8130Signed, TxEip8130};
use base_common_network::Base;
use base_common_rpc_types::BaseTransactionReceipt;
use base_optimism_rpc::OptimismRollupProviderExt;
use base_prover_service_client::{ProofRequesterClient, ProverServiceClientConfig};
use base_prover_service_protocol::{
    ExecutionStats, GetProofRequest, GetProofResponse, ProofRequest, ProofRequestKind, ProofResult,
    ProofStatus, ProveBlockRangeRequest, ZkBackend, ZkProofRequest, ZkVm,
};
use base_system_tests::{
    ANVIL_ACCOUNT_1, InProcessProverService, InProcessZkHost, SystemTestProviderExt,
    SystemTestStackBuilder,
};
use eyre::{Result, WrapErr, ensure};
use nanoid::nanoid;
use tokio::time::{sleep, timeout};
use url::Url;

/// EIP-8130 transaction type byte.
const EIP8130_TX_TYPE: u8 = 0x79;
const SAFE_L2_TIMEOUT: Duration = Duration::from_secs(120);
const SAFE_L2_POLL_INTERVAL: Duration = Duration::from_millis(500);
const PROOF_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const PROOF_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Mines a minimal EOA-path EIP-8130 transaction on the Zenith system-test stack.
#[tokio::test]
async fn eip8130_transaction_is_mined() -> Result<()> {
    let (_system, provider) = zenith::start_zenith_system().await?;
    let (_tx_hash, receipt) = send_minimal_eip8130(&provider).await?;

    assert!(receipt.status(), "EIP-8130 transaction receipt must report success");
    assert_eq!(
        receipt.inner.inner.receipt.ty(),
        EIP8130_TX_TYPE,
        "mined receipt must report type 0x79"
    );
    assert_eq!(
        receipt.payer,
        Some(ANVIL_ACCOUNT_1.address),
        "self-pay receipt payer must be the sender"
    );

    Ok(())
}

/// Dry-run SP1-executes the block that contains a type `0x79` transaction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "SP1 dry-run execute is too slow for merge-queue; run with --release"]
async fn eip8130_block_dry_run_proves() -> Result<()> {
    ensure!(
        !cfg!(debug_assertions),
        "SP1 dry-run execute does not finish in the unoptimized test profile. Re-run with --release:\n\
         cargo nextest run --release -p base-system-tests --run-ignored all \\\n\
         -E 'test(eip8130_block_dry_run)'"
    );

    let (system, provider) =
        zenith::start_zenith_stack(SystemTestStackBuilder::new().with_force_batch_submission())
            .await?;
    let (_tx_hash, receipt) = send_minimal_eip8130(&provider).await?;
    let block_number =
        receipt.block_number().expect("mined EIP-8130 transaction must have a block");

    let rollup_provider =
        RootProvider::<Base>::new_http(system.l2_stack().builder_consensus_rpc_url());
    wait_for_safe_l2(&rollup_provider, block_number).await?;

    let service = InProcessProverService::start().await?;
    let _host = InProcessZkHost::start(&system, service.url()).await?;

    // Pin rollup `head_l1`; local L1 finality lags the batches that made this block safe.
    let l1_head = rollup_provider.optimism_sync_status().await?.head_l1.hash;
    let stats =
        prove_block_range_with_dry_run_stats(service.url().clone(), block_number, l1_head).await?;
    ensure!(
        stats.total_instruction_cycles > 0,
        "dry-run of an EIP-8130 block must report non-zero instruction cycles"
    );

    Ok(())
}

async fn send_minimal_eip8130(
    provider: &RootProvider<Base>,
) -> Result<(B256, BaseTransactionReceipt)> {
    let signer = PrivateKeySigner::from_bytes(&ANVIL_ACCOUNT_1.private_key)
        .wrap_err("Failed to parse system test private key")?;
    balance::wait_for_balance(provider, signer.address()).await?;

    let nonce_sequence = provider.get_transaction_count(signer.address()).await?;
    let tx = TxEip8130 {
        chain_id: common::L2_CHAIN_ID,
        sender: None,
        nonce_key: U256::ZERO,
        nonce_sequence,
        valid_after: 0,
        valid_before: 0,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 1_000_000_000,
        gas_limit: 200_000,
        account_changes: Vec::new(),
        calls: Vec::new(),
        metadata: Bytes::new(),
        payer: None,
    };

    let sender_auth: Bytes = signer.sign_hash_sync(&tx.sender_signature_hash())?.as_bytes().into();
    let signed = Eip8130Signed::new(tx, sender_auth, Bytes::new());
    let tx_hash = *signed.hash();
    let raw: Bytes = signed.encoded_2718().into();
    ensure!(
        raw.first() == Some(&EIP8130_TX_TYPE),
        "encoded transaction must carry the 0x79 type byte"
    );

    let pending = provider
        .send_raw_transaction(&raw)
        .await
        .wrap_err("Failed to send EIP-8130 transaction")?;
    ensure!(*pending.tx_hash() == tx_hash, "sent EIP-8130 hash must match the signed envelope");
    drop(pending);
    let receipt = provider
        .wait_for_receipt(tx_hash, balance::TX_RECEIPT_TIMEOUT)
        .await
        .wrap_err("EIP-8130 receipt timed out")?;
    Ok((tx_hash, receipt))
}

async fn wait_for_safe_l2(provider: &RootProvider<Base>, block_number: u64) -> Result<()> {
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
                "timed out waiting for EIP-8130 block {block_number} to become safe \
                 (safe_l2={}, unsafe_l2={})",
                status.safe_l2.number,
                status.unsafe_l2.number
            );
        }
    }
}

async fn prove_block_range_with_dry_run_stats(
    prover_url: Url,
    block_number: u64,
    l1_head: B256,
) -> Result<ExecutionStats> {
    let start_block_number = block_number
        .checked_sub(1)
        .ok_or_else(|| eyre::eyre!("cannot prove genesis block with one-block range"))?;
    let client_config = ProverServiceClientConfig::new(prover_url.as_str())
        .with_request_timeout(Duration::from_secs(30));
    let client = ProofRequesterClient::connect(&client_config)?;
    let session_id = format!("eip8130-zk-dry-run-{}", nanoid!());
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
