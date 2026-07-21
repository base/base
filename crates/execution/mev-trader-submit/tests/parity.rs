//! TS byte-parity: the Rust rung-1 assembler must produce EXACTLY the bytes the
//! `TypeScript` verification prototype produces for the same input.
//!
//! Goldens are independently reconstructed with canonical local ABI/RLP encoding
//! for the same inputs as the repo's TypeScript `encodeFunctionData` and
//! `serializeMeasurementTxBytes` paths (fixed invalid high-s dummy signature).
#![cfg(feature = "phase-b")]

mod support;

use alloy_primitives::{U256, hex, keccak256};
use base_mev_trader::ExactProtocol;
use mev_trader_submit::assembler::{
    AssembleInput, BundleTxRef, HopExecutionParams, TwoChannelInput, assemble_unsigned_atomic_tx,
    build_two_channel_dummy_assembly, encode_executor_calldata,
};
use support::{ADAPTER, EXECUTOR, FIRST_MIN_OUT, backrun_plan, victim_with_priority};

/// Canonical ABI golden for both hops at `feeBps=30` (`fee_pips` 3000), with
/// each V2 hop's `fundingTarget` equal to its pool.
const CALLDATA_GOLDEN: &str = "0x3b83f27200000000000000000000000000000000000000000000000000000000000000a100000000000000000000000000000000000000000000000000000000000000f1000000000000000000000000420000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000c0000000000000000000000000000000000000000000000000000000000000001e00000000000000000000000000000000000000000000000000000000499602d200000000000000000000000000000000000000000000000000000000000000f100000000000000000000000000000000000000000000000000000000000000a100000000000000000000000000000000000000000000000000000000000000f200000000000000000000000000000000000000000000000000000000000000c00000000000000000000000004200000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000001e000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000f20000000000000000000000000000000000000000000000000de0b6b3a76400000000000000000000000000000000000000000000000000000e043da6172500000000000000000000000000000000000000000000000000000000000000bc614e";

/// Canonical ABI golden where the first hop is V3 (`feeBps=0`, adapter-funded)
/// and the second hop is V2 (pool-funded).
const V3_CALLDATA_GOLDEN: &str = "0x3b83f27200000000000000000000000000000000000000000000000000000000000000a100000000000000000000000000000000000000000000000000000000000000f1000000000000000000000000420000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000499602d200000000000000000000000000000000000000000000000000000000000000a100000000000000000000000000000000000000000000000000000000000000a100000000000000000000000000000000000000000000000000000000000000f200000000000000000000000000000000000000000000000000000000000000c00000000000000000000000004200000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000001e000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000f20000000000000000000000000000000000000000000000000de0b6b3a76400000000000000000000000000000000000000000000000000000e043da6172500000000000000000000000000000000000000000000000000000000000000bc614e";

/// Canonical EIP-1559 RLP golden for the assembled unsigned tx at
/// maxPriorityFeePerGas=37 with the fixed invalid high-s dummy signature.
const DUMMY_RAW_TX_GOLDEN: &str = "0x02f9028f8221058025843b9aca00831e848094200000000000000000000000000000000000000280b902243b83f27200000000000000000000000000000000000000000000000000000000000000a100000000000000000000000000000000000000000000000000000000000000f1000000000000000000000000420000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000c0000000000000000000000000000000000000000000000000000000000000001e00000000000000000000000000000000000000000000000000000000499602d200000000000000000000000000000000000000000000000000000000000000f100000000000000000000000000000000000000000000000000000000000000a100000000000000000000000000000000000000000000000000000000000000f200000000000000000000000000000000000000000000000000000000000000c00000000000000000000000004200000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000001e000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000f20000000000000000000000000000000000000000000000000de0b6b3a76400000000000000000000000000000000000000000000000000000e043da6172500000000000000000000000000000000000000000000000000000000000000bc614ec001a05fdab2bc3e0846351de15a51b4f354bf4a4ce227302de002ac790bacef8ba802a0adccfdc48b0427d6d60ddfacca470a52f6924a603539118d356c152d1f0b5986";

fn hop_params() -> [HopExecutionParams; 2] {
    [
        HopExecutionParams { adapter: ADAPTER, min_amount_out: U256::from(FIRST_MIN_OUT) },
        HopExecutionParams { adapter: ADAPTER, min_amount_out: U256::from(1u64) },
    ]
}

#[test]
fn calldata_matches_ts_encode_function_data() {
    let (victim_raw, victim_hash) = victim_with_priority(37);
    let plan = backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim_hash);
    let input = AssembleInput {
        plan: &plan,
        current_frame: support::matching_frame(&plan),
        executor: EXECUTOR,
        hops: hop_params(),
        chain_id: 8453,
        nonce: 0,
        gas: 2_000_000,
        max_fee_per_gas: 1_000_000_000,
        valid_until_block: 12_345_678,
        victim_raw_tx: &victim_raw,
        victim_tx_hash: victim_hash,
        expected_victim_priority_fee: Some(37),
    };
    let calldata = encode_executor_calldata(&input).expect("calldata");
    assert_eq!(hex::encode_prefixed(&calldata), CALLDATA_GOLDEN);
}

#[test]
fn v3_first_hop_canonicalizes_fee_bps_to_zero() {
    let (_, victim_hash) = victim_with_priority(37);
    // First hop is a self-applying V3 pool: feeBps must encode as 0 regardless of
    // the re-derived fee_pips.
    let plan = backrun_plan([ExactProtocol::UniswapV3, ExactProtocol::UniswapV2], victim_hash);
    let (victim_raw, _) = victim_with_priority(37);
    let input = AssembleInput {
        plan: &plan,
        current_frame: support::matching_frame(&plan),
        executor: EXECUTOR,
        hops: hop_params(),
        chain_id: 8453,
        nonce: 0,
        gas: 2_000_000,
        max_fee_per_gas: 1_000_000_000,
        valid_until_block: 12_345_678,
        victim_raw_tx: &victim_raw,
        victim_tx_hash: keccak256(&victim_raw),
        expected_victim_priority_fee: Some(37),
    };
    let calldata = encode_executor_calldata(&input).expect("calldata");
    assert_eq!(hex::encode_prefixed(&calldata), V3_CALLDATA_GOLDEN);
}

#[test]
fn dummy_raw_tx_matches_ts_serialize_measurement_tx_bytes() {
    let (victim_raw, victim_hash) = victim_with_priority(37);
    let plan = backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim_hash);
    let input = AssembleInput {
        plan: &plan,
        current_frame: support::matching_frame(&plan),
        executor: EXECUTOR,
        hops: hop_params(),
        chain_id: 8453,
        nonce: 0,
        gas: 2_000_000,
        max_fee_per_gas: 1_000_000_000,
        valid_until_block: 12_345_678,
        victim_raw_tx: &victim_raw,
        victim_tx_hash: victim_hash,
        expected_victim_priority_fee: Some(37),
    };
    let assembled = assemble_unsigned_atomic_tx(&input).expect("assembled");
    assert!(assembled.non_broadcastable);
    assert_eq!(assembled.victim_max_priority_fee_per_gas, 37);
    assert_eq!(hex::encode_prefixed(&assembled.dummy_signed_raw_tx), DUMMY_RAW_TX_GOLDEN);
    // The dummy envelope must carry the exact fixed invalid high-s dummy signature
    // tail (non-broadcastable on EIP-2 high-s grounds).
    assert!(
        DUMMY_RAW_TX_GOLDEN
            .ends_with("adccfdc48b0427d6d60ddfacca470a52f6924a603539118d356c152d1f0b5986")
    );
}

#[test]
fn two_channel_dummy_assembly_puts_victim_hash_first_with_zero_bid() {
    let (victim_raw, victim_hash) = victim_with_priority(37);
    let plan = backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim_hash);
    let input = AssembleInput {
        plan: &plan,
        current_frame: support::matching_frame(&plan),
        executor: EXECUTOR,
        hops: hop_params(),
        chain_id: 8453,
        nonce: 0,
        gas: 2_000_000,
        max_fee_per_gas: 1_000_000_000,
        valid_until_block: 12_345_678,
        victim_raw_tx: &victim_raw,
        victim_tx_hash: victim_hash,
        expected_victim_priority_fee: Some(37),
    };
    let assembled = assemble_unsigned_atomic_tx(&input).expect("assembled");
    let two_channel = build_two_channel_dummy_assembly(&TwoChannelInput {
        victim_raw_tx: &victim_raw,
        victim_tx_hash: victim_hash,
        dummy_raw_backrun: &assembled.dummy_signed_raw_tx,
    })
    .expect("two channel");

    // Inclusion channel: [victim_raw, dummy_backrun] (eth_sendRawTransaction shape).
    assert_eq!(two_channel.direct[0], victim_raw);
    assert_eq!(two_channel.direct[1], assembled.dummy_signed_raw_tx);
    // Attribution channel: eth_sendBundle[victim_hash, dummy_backrun], bid 0.
    assert_eq!(two_channel.attribution.method, "eth_sendBundle");
    assert_eq!(two_channel.attribution.bid_wei, U256::ZERO);
    assert_eq!(two_channel.attribution.txs[0], BundleTxRef::Hash(victim_hash));
    assert_eq!(
        two_channel.attribution.txs[1],
        BundleTxRef::Raw(assembled.dummy_signed_raw_tx.clone())
    );

    // A non-dummy (real) backrun must be rejected by the attribution builder.
    let real =
        mev_trader_submit::signer::sign_ephemeral_atomic_tx(&assembled.unsigned_tx).expect("real");
    assert!(
        build_two_channel_dummy_assembly(&TwoChannelInput {
            victim_raw_tx: &victim_raw,
            victim_tx_hash: victim_hash,
            dummy_raw_backrun: &real.raw_backrun,
        })
        .is_err(),
        "attribution accepted a non-dummy backrun"
    );
}
