//! Runs the full derivation pipeline starting from Base mainnet block #43127819 and steps it
//! until it produces payload attributes for block #43127820, then compares every field of the
//! produced L1BlockInfo deposit against the actual on-chain values.
//!
//! # Required environment variables
//!
//! * `L1_RPC_URL`     — Ethereum mainnet JSON-RPC endpoint
//! * `BEACON_RPC_URL` — Ethereum beacon-chain REST API endpoint
//!
//! # Optional environment variables
//!
//! * `L2_RPC_URL` — Base mainnet JSON-RPC endpoint
//!                  (defaults to `https://c3-chainproxy-base-mainnet-archive.cbhq.net`)
//!
//! The test returns early (passes trivially) when the required env vars are absent, so it is
//! safe to run in CI environments that do not have RPC access.

use std::sync::Arc;

use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{B64, Bytes};
use alloy_provider::RootProvider;
use alloy_rpc_types_engine::PayloadAttributes;
use base_alloy_consensus::OpTxEnvelope;
use base_alloy_network::Base;
use base_alloy_rpc_types_engine::OpPayloadAttributes;
use base_consensus_derive::{ChainProvider, Pipeline, PipelineErrorKind, StepResult};
use base_consensus_providers::{
    AlloyChainProvider, AlloyL2ChainProvider, OnlineBeaconClient, OnlineBlobProvider,
    OnlinePipeline,
};
use base_consensus_registry::Registry;
use base_protocol::{BatchValidationProvider, L1BlockInfoTx};
use url::Url;

const DEFAULT_L2_RPC: &str = "https://c3-chainproxy-base-mainnet-archive.cbhq.net";
const L2_PARENT_NUMBER: u64 = 43_127_819;
const TARGET_BLOCK_NUMBER: u64 = 43_127_820;

/// Steps the derivation pipeline from block #43127819 until it produces payload attributes for
/// block #43127820, then validates all L1BlockInfo fields against the actual on-chain values.
#[tokio::test]
async fn test_payload_attributes_block_43127820() {
    let l1_rpc = match std::env::var("L1_RPC_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("skipping test_payload_attributes_block_43127820: L1_RPC_URL not set");
            return;
        }
    };
    let beacon_url = match std::env::var("BEACON_RPC_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("skipping test_payload_attributes_block_43127820: BEACON_RPC_URL not set");
            return;
        }
    };
    let l2_rpc = std::env::var("L2_RPC_URL").unwrap_or_else(|_| DEFAULT_L2_RPC.to_string());

    // Real configs from the registry — no hardcoding.
    let rollup_cfg =
        Arc::new(Registry::rollup_config(8453).expect("Base mainnet rollup config").clone());
    let l1_cfg =
        Arc::new(Registry::l1_config(1).expect("Ethereum mainnet L1 chain config").clone());

    // L2 provider (Base network).
    let l2_url: Url = l2_rpc.parse().expect("valid L2 RPC URL");
    let l2_root = RootProvider::<Base>::new_http(l2_url);
    let mut l2_chain_provider =
        AlloyL2ChainProvider::new(l2_root, Arc::clone(&rollup_cfg), 1024);

    // L1 provider (Ethereum mainnet).
    let l1_url: Url = l1_rpc.parse().expect("valid L1 RPC URL");
    let mut l1_chain_provider = AlloyChainProvider::new_http(l1_url, 1024);

    // Blob provider (Ethereum beacon chain).
    let blob_provider =
        OnlineBlobProvider::init(OnlineBeaconClient::new_http(beacon_url)).await;

    // Fetch the L2 safe head (block #43127819).
    let l2_safe_head = l2_chain_provider
        .l2_block_info_by_number(L2_PARENT_NUMBER)
        .await
        .expect("fetch L2 parent block info");

    // Fetch the L1 origin referenced by the safe head.
    let l1_origin = l1_chain_provider
        .block_info_by_number(l2_safe_head.l1_origin.number)
        .await
        .expect("fetch L1 origin block info");

    // Construct and initialise the full derivation pipeline.
    let mut pipeline = OnlinePipeline::new(
        Arc::clone(&rollup_cfg),
        Arc::clone(&l1_cfg),
        l2_safe_head,
        l1_origin,
        blob_provider,
        l1_chain_provider,
        l2_chain_provider.clone(),
    )
    .await
    .expect("construct derivation pipeline");

    // Step the pipeline until it produces attributes for the next L2 block.
    // Temporary errors mean "not enough data yet — retry", matching the real driver behaviour.
    let produced = loop {
        match pipeline.step(l2_safe_head).await {
            StepResult::PreparedAttributes => break pipeline.next().expect("prepared attributes"),
            StepResult::AdvancedOrigin => continue,
            StepResult::OriginAdvanceErr(PipelineErrorKind::Temporary(_))
            | StepResult::StepFailed(PipelineErrorKind::Temporary(_)) => continue,
            StepResult::OriginAdvanceErr(e) => panic!("origin advance error: {e:?}"),
            StepResult::StepFailed(e) => panic!("pipeline step failed: {e:?}"),
        }
    };

    // Decode the L1BlockInfo deposit from the produced payload attributes.
    let produced_txs = produced.attributes.transactions.clone().expect("transactions present");
    assert!(!produced_txs.is_empty(), "produced attributes must contain the L1 info deposit");
    let produced_envelope =
        OpTxEnvelope::decode_2718(&mut produced_txs[0].as_ref()).expect("valid 2718 deposit");
    let OpTxEnvelope::Deposit(produced_deposit) = produced_envelope else {
        panic!("first produced tx must be a deposit");
    };
    let produced_info =
        L1BlockInfoTx::decode_calldata(&produced_deposit.input).expect("valid produced L1BlockInfo");

    // Fetch the real on-chain block #43127820 and decode its L1BlockInfo deposit.
    let target_block = l2_chain_provider
        .block_by_number(TARGET_BLOCK_NUMBER)
        .await
        .expect("fetch target block #43127820");
    let OpTxEnvelope::Deposit(expected_deposit) = &target_block.body.transactions[0] else {
        panic!("first tx of block #43127820 must be a deposit");
    };
    let expected_info = L1BlockInfoTx::decode_calldata(&expected_deposit.input)
        .expect("valid expected L1BlockInfo");

    // Reconstruct expected OpPayloadAttributes from the on-chain block for symmetric comparison.
    let expected_transactions: Vec<Bytes> = target_block
        .body
        .transactions
        .iter()
        .map(|tx| {
            let mut buf = Vec::new();
            tx.encode_2718(&mut buf);
            Bytes::from(buf)
        })
        .collect();

    let (expected_eip_1559_params, expected_min_base_fee) =
        match target_block.header.extra_data.len() {
            9 => (Some(B64::from_slice(&target_block.header.extra_data[1..9])), None),
            17 => {
                let min_base_fee = u64::from_be_bytes(
                    target_block.header.extra_data[9..17].try_into().unwrap(),
                );
                (
                    Some(B64::from_slice(&target_block.header.extra_data[1..9])),
                    Some(min_base_fee),
                )
            }
            _ => (None, None),
        };

    let expected_attributes = OpPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp: target_block.header.timestamp,
            prev_randao: target_block.header.mix_hash,
            suggested_fee_recipient: target_block.header.beneficiary,
            withdrawals: target_block.body.withdrawals.clone().map(|w| w.0),
            parent_beacon_block_root: target_block.header.parent_beacon_block_root,
        },
        transactions: Some(expected_transactions),
        no_tx_pool: Some(true),
        gas_limit: Some(target_block.header.gas_limit),
        eip_1559_params: expected_eip_1559_params,
        min_base_fee: expected_min_base_fee,
    };

    // ── Compare L1BlockInfo fields ──────────────────────────────────────────────────────────
    let mut all_match = true;
    let mut check = |name: &str, produced: &dyn std::fmt::Debug, expected: &dyn std::fmt::Debug, eq: bool| {
        if eq {
            eprintln!("  OK  {name}: {produced:?}");
        } else {
            eprintln!("  FAIL {name}: produced={produced:?}  expected={expected:?}");
            all_match = false;
        }
    };

    eprintln!("\n=== L1BlockInfo field comparison ===");
    check("sequence_number",
        &produced_info.sequence_number(), &expected_info.sequence_number(),
        produced_info.sequence_number() == expected_info.sequence_number());
    check("l1_base_fee",
        &produced_info.l1_base_fee(), &expected_info.l1_base_fee(),
        produced_info.l1_base_fee() == expected_info.l1_base_fee());
    check("blob_base_fee",
        &produced_info.blob_base_fee(), &expected_info.blob_base_fee(),
        produced_info.blob_base_fee() == expected_info.blob_base_fee());
    check("base_fee_scalar",
        &produced_info.l1_fee_scalar(), &expected_info.l1_fee_scalar(),
        produced_info.l1_fee_scalar() == expected_info.l1_fee_scalar());
    check("blob_base_fee_scalar",
        &produced_info.blob_base_fee_scalar(), &expected_info.blob_base_fee_scalar(),
        produced_info.blob_base_fee_scalar() == expected_info.blob_base_fee_scalar());
    check("batcher_address",
        &produced_info.batcher_address(), &expected_info.batcher_address(),
        produced_info.batcher_address() == expected_info.batcher_address());
    check("operator_fee_scalar",
        &produced_info.operator_fee_scalar(), &expected_info.operator_fee_scalar(),
        produced_info.operator_fee_scalar() == expected_info.operator_fee_scalar());
    check("operator_fee_constant",
        &produced_info.operator_fee_constant(), &expected_info.operator_fee_constant(),
        produced_info.operator_fee_constant() == expected_info.operator_fee_constant());
    check("da_footprint_gas_scalar",
        &produced_info.da_footprint(), &expected_info.da_footprint(),
        produced_info.da_footprint() == expected_info.da_footprint());

    // ── Compare full payload attributes ──────────────────────────────────────────────────────
    eprintln!("\n=== Payload attributes comparison ===");
    let pa = &produced.attributes;
    let ea = &expected_attributes;

    check("timestamp",
        &pa.payload_attributes.timestamp, &ea.payload_attributes.timestamp,
        pa.payload_attributes.timestamp == ea.payload_attributes.timestamp);
    check("prev_randao",
        &pa.payload_attributes.prev_randao, &ea.payload_attributes.prev_randao,
        pa.payload_attributes.prev_randao == ea.payload_attributes.prev_randao);
    check("suggested_fee_recipient",
        &pa.payload_attributes.suggested_fee_recipient, &ea.payload_attributes.suggested_fee_recipient,
        pa.payload_attributes.suggested_fee_recipient == ea.payload_attributes.suggested_fee_recipient);
    check("gas_limit",
        &pa.gas_limit, &ea.gas_limit,
        pa.gas_limit == ea.gas_limit);
    check("eip_1559_params",
        &pa.eip_1559_params, &ea.eip_1559_params,
        pa.eip_1559_params == ea.eip_1559_params);
    check("min_base_fee",
        &pa.min_base_fee, &ea.min_base_fee,
        pa.min_base_fee == ea.min_base_fee);
    check("no_tx_pool",
        &pa.no_tx_pool, &ea.no_tx_pool,
        pa.no_tx_pool == ea.no_tx_pool);
    check("withdrawals",
        &pa.payload_attributes.withdrawals, &ea.payload_attributes.withdrawals,
        pa.payload_attributes.withdrawals == ea.payload_attributes.withdrawals);
    check("parent_beacon_block_root",
        &pa.payload_attributes.parent_beacon_block_root, &ea.payload_attributes.parent_beacon_block_root,
        pa.payload_attributes.parent_beacon_block_root == ea.payload_attributes.parent_beacon_block_root);

    // Compare transaction count and each transaction.
    let produced_txs_ref = pa.transactions.as_ref().expect("produced txs");
    let expected_txs_ref = ea.transactions.as_ref().expect("expected txs");
    check("transaction_count",
        &produced_txs_ref.len(), &expected_txs_ref.len(),
        produced_txs_ref.len() == expected_txs_ref.len());

    let tx_count = std::cmp::min(produced_txs_ref.len(), expected_txs_ref.len());
    let mut tx_mismatches = 0;
    for i in 0..tx_count {
        if produced_txs_ref[i] != expected_txs_ref[i] {
            tx_mismatches += 1;
            if tx_mismatches <= 5 {
                eprintln!("  FAIL transaction[{i}]: produced != expected (first differing byte shown)");
            }
        }
    }
    if tx_mismatches > 0 {
        eprintln!("  FAIL {tx_mismatches}/{tx_count} transactions differ");
        all_match = false;
    } else {
        eprintln!("  OK  all {tx_count} transactions match byte-for-byte");
    }

    // Write output files.
    let outdir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testdata");
    std::fs::create_dir_all(&outdir).expect("create testdata dir");
    std::fs::write(
        outdir.join("produced_payload_attributes_block_43127820.txt"),
        format!("{:#?}\n\nL1BlockInfo:\n{produced_info:#?}", produced.attributes),
    )
    .expect("write produced payload attributes");
    std::fs::write(
        outdir.join("expected_payload_attributes_block_43127820.txt"),
        format!("{:#?}\n\nL1BlockInfo:\n{expected_info:#?}", expected_attributes),
    )
    .expect("write expected payload attributes");

    if all_match {
        eprintln!("\n=== RESULT: ALL FIELDS MATCH ===");
    } else {
        eprintln!("\n=== RESULT: MISMATCH DETECTED ===");
    }

    // Final assertion: all payload attributes must match on-chain values.
    assert!(all_match, "payload attributes do not match on-chain values (see field comparison above)");
}
