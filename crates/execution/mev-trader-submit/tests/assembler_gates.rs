//! R8 fee-SOURCE + integrity gates: the assembler derives the executor `feeBps`
//! SOLELY from the carried, digest-bound `BackrunHop::fee_pips`, and refuses to
//! emit calldata for a tampered plan (digest self-check) or a stale/wrong-frame
//! plan (frame-identity check) — two INDEPENDENT fail-closed gates. Also locks the
//! fee-parity §3.4 conversion guards. All local/offline; no signer, submit, or key.
#![cfg(feature = "phase-b")]

mod support;

use alloy_primitives::{B256, U256, keccak256};
use alloy_rpc_types_engine::PayloadId;
use base_mev_trader::{BackrunPlan, ExactProtocol, MeasurementContext};
use mev_trader_submit::assembler::{
    AssembleError, AssembleInput, HopExecutionParams, encode_executor_calldata,
};
use mev_trader_submit::fee::{FeeParityError, fee_bps_for_executor};
use support::{ADAPTER, EXECUTOR, backrun_plan, finalize_plan_digest, matching_frame};

/// Build an `AssembleInput` that exercises ONLY the calldata path (victim-envelope
/// fields are unused by `encode_executor_calldata` and left minimal here).
fn calldata_input<'a>(plan: &'a BackrunPlan, current_frame: MeasurementContext) -> AssembleInput<'a> {
    AssembleInput {
        plan,
        current_frame,
        executor: EXECUTOR,
        hops: [
            HopExecutionParams { adapter: ADAPTER, min_amount_out: U256::from(1u64) },
            HopExecutionParams { adapter: ADAPTER, min_amount_out: U256::from(1u64) },
        ],
        chain_id: 8453,
        nonce: 0,
        gas: 2_000_000,
        max_fee_per_gas: 1_000_000_000,
        valid_until_block: 12_345_678,
        victim_raw_tx: &[],
        victim_tx_hash: plan.victim,
        expected_victim_priority_fee: None,
    }
}

#[test]
fn carried_fee_pips_drives_the_executor_feebps() {
    // Two plans identical except route[0].fee_pips must produce DIFFERENT calldata:
    // the ABI feeBps is read from the carried plan fee, not a caller/constant value.
    let victim = keccak256(b"carry");
    let plan_low = backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim);
    let low = encode_executor_calldata(&calldata_input(&plan_low, matching_frame(&plan_low)))
        .expect("low-fee calldata");

    let mut plan_high = backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim);
    plan_high.route[0].fee_pips = 6_000; // 0.30% -> 0.60% (feeBps 30 -> 60)
    finalize_plan_digest(&mut plan_high);
    let high = encode_executor_calldata(&calldata_input(&plan_high, matching_frame(&plan_high)))
        .expect("high-fee calldata");

    assert_ne!(low, high, "carried fee_pips did not flow into the executor feeBps");
}

#[test]
fn field_or_fee_tamper_fails_the_digest_self_check_and_emits_nothing() {
    let victim = keccak256(b"tamper");
    let sealed = backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim);

    // (a) Tampering a bound field (victim) after sealing flips the digest.
    let mut victim_tampered = sealed.clone();
    victim_tampered.victim = keccak256(b"not-the-victim");
    assert_eq!(
        encode_executor_calldata(&calldata_input(&victim_tampered, matching_frame(&sealed))),
        Err(AssembleError::DigestMismatch),
    );

    // (b) Tampering ONLY the fee after sealing also flips the digest (R8: fee is in
    // the digest preimage), so a mispriced fee can never reach the executor.
    let mut fee_tampered = sealed.clone();
    fee_tampered.route[1].fee_pips = 500; // was 3000
    assert_eq!(
        encode_executor_calldata(&calldata_input(&fee_tampered, matching_frame(&sealed))),
        Err(AssembleError::DigestMismatch),
    );
}

#[test]
fn stale_or_wrong_frame_fails_the_identity_gate_and_emits_nothing() {
    let victim = keccak256(b"frame");
    let plan = backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim);

    // Non-vacuous: the EXACT current frame passes and produces calldata.
    assert!(encode_executor_calldata(&calldata_input(&plan, matching_frame(&plan))).is_ok());

    // Same parent_hash + block, but a STALE flashblock generation (later
    // predecessor_index) must be rejected — digest alone cannot catch this.
    let stale_predecessor = MeasurementContext {
        predecessor_index: plan.predecessor_index + 1,
        ..matching_frame(&plan)
    };
    assert_eq!(
        encode_executor_calldata(&calldata_input(&plan, stale_predecessor)),
        Err(AssembleError::FrameIdentityMismatch),
    );

    // Same parent_hash, but a different payload_id generation is also rejected.
    let stale_payload =
        MeasurementContext { payload_id: PayloadId::new([9u8; 8]), ..matching_frame(&plan) };
    assert_eq!(
        encode_executor_calldata(&calldata_input(&plan, stale_payload)),
        Err(AssembleError::FrameIdentityMismatch),
    );

    // Wrong parent (different pending state) is rejected.
    let wrong_parent =
        MeasurementContext { parent_hash: B256::repeat_byte(0xee), ..matching_frame(&plan) };
    assert_eq!(
        encode_executor_calldata(&calldata_input(&plan, wrong_parent)),
        Err(AssembleError::FrameIdentityMismatch),
    );

    // Wrong block number is rejected.
    let wrong_block =
        MeasurementContext { block_number: plan.block_number + 1, ..matching_frame(&plan) };
    assert_eq!(
        encode_executor_calldata(&calldata_input(&plan, wrong_block)),
        Err(AssembleError::FrameIdentityMismatch),
    );
}

#[test]
fn fee_parity_conversion_guards_are_total_and_fail_closed() {
    // Self-applying pools (UniswapV3, AerodromeStable) canonicalize to feeBps 0
    // regardless of the sizing fee (the pool applies its own fee on-chain).
    assert_eq!(fee_bps_for_executor(ExactProtocol::UniswapV3, 3_000), Ok(0));
    assert_eq!(fee_bps_for_executor(ExactProtocol::UniswapV3, 500), Ok(0));
    assert_eq!(fee_bps_for_executor(ExactProtocol::AerodromeStable, 3_000), Ok(0));

    // Constant-product pools (UniswapV2, AerodromeVolatile) convert losslessly.
    assert_eq!(fee_bps_for_executor(ExactProtocol::UniswapV2, 3_000), Ok(30));
    assert_eq!(fee_bps_for_executor(ExactProtocol::AerodromeVolatile, 100), Ok(1));

    // Fractional bps (fee_pips % 100 != 0) on a constant-product pool is rejected.
    assert_eq!(
        fee_bps_for_executor(ExactProtocol::UniswapV2, 3_050),
        Err(FeeParityError::FractionalBps { fee_pips: 3_050 }),
    );
    assert_eq!(
        fee_bps_for_executor(ExactProtocol::AerodromeVolatile, 150),
        Err(FeeParityError::FractionalBps { fee_pips: 150 }),
    );

    // The out-of-range guard is checked BEFORE the protocol branch, so it fires for
    // every protocol — including the self-applying ones.
    assert_eq!(
        fee_bps_for_executor(ExactProtocol::UniswapV2, 1_000_001),
        Err(FeeParityError::OutOfRange { fee_pips: 1_000_001 }),
    );
    assert_eq!(
        fee_bps_for_executor(ExactProtocol::UniswapV3, 1_000_001),
        Err(FeeParityError::OutOfRange { fee_pips: 1_000_001 }),
    );
    // Exactly the denominator is in range (== 100%, not >).
    assert_eq!(fee_bps_for_executor(ExactProtocol::UniswapV2, 1_000_000), Ok(10_000));
}

#[test]
fn fee_parity_guard_failure_aborts_calldata_emit() {
    // The §3.4 guard is enforced THROUGH the assembler: a fractional or out-of-range
    // carried fee (sealed into the digest) yields an error, never mispriced bytes.
    let victim = keccak256(b"guard");

    let mut fractional =
        backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim);
    fractional.route[0].fee_pips = 3_050; // fractional bps
    finalize_plan_digest(&mut fractional);
    assert_eq!(
        encode_executor_calldata(&calldata_input(&fractional, matching_frame(&fractional))),
        Err(AssembleError::FeeParity(FeeParityError::FractionalBps { fee_pips: 3_050 })),
    );

    let mut out_of_range =
        backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim);
    out_of_range.route[1].fee_pips = 1_000_001; // > denominator
    finalize_plan_digest(&mut out_of_range);
    assert_eq!(
        encode_executor_calldata(&calldata_input(&out_of_range, matching_frame(&out_of_range))),
        Err(AssembleError::FeeParity(FeeParityError::OutOfRange { fee_pips: 1_000_001 })),
    );

    // A self-applying first hop (UniswapV3) canonicalizes to feeBps 0 and emits.
    let v3 = backrun_plan([ExactProtocol::UniswapV3, ExactProtocol::UniswapV2], victim);
    assert!(encode_executor_calldata(&calldata_input(&v3, matching_frame(&v3))).is_ok());
}
