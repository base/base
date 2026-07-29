//! R8 fee-SOURCE + integrity gates: the assembler derives the executor `feeBps`
//! SOLELY from the carried, digest-bound `BackrunHop::fee_pips`, and refuses to
//! emit calldata for a tampered plan (digest self-check) or a stale/wrong-frame
//! plan (frame-identity check) — two INDEPENDENT fail-closed gates. Also locks the
//! fee-parity §3.4 conversion guards. All local/offline; no signer, submit, or key.
#![cfg(feature = "phase-b")]

mod support;

use alloy_primitives::{B256, U256, aliases::U24, keccak256};
use alloy_rpc_types_engine::PayloadId;
use alloy_sol_types::SolCall;
use base_mev_trader::{BackrunPlan, ExactProtocol, MeasurementContext};
use mev_trader_submit::{
    PriorityEconomicsAuthority,
    assembler::{
        AssembleError, AssembleInput, HopExecutionParams, assemble_unsigned_atomic_tx,
        assemble_validated, encode_executor_calldata,
    },
    fee::{FeeParityError, fee_bps_for_executor},
};
use support::{
    ADAPTER, EXECUTOR, backrun_plan, finalize_plan_digest, matching_frame, victim_with_priority,
};

/// The executor entrypoint ABI — used ONLY to decode the emitted calldata and read
/// back the derived `feeBps` (this is a test-side decoder; production never decodes).
mod exec_abi {
    alloy_sol_types::sol! {
        struct SwapHop {
            address adapter;
            address pool;
            address tokenIn;
            address tokenOut;
            uint24 feeBps;
            uint256 minAmountOut;
            address fundingTarget;
        }
        function executeBlinkOfaAtomic(
            SwapHop firstHop,
            SwapHop secondHop,
            uint256 amountIn,
            uint256 minFinalAmount,
            uint256 validUntilBlock
        );
    }
}

/// Build an `AssembleInput` that exercises ONLY the calldata path (victim-envelope
/// fields are unused by `encode_executor_calldata` and left minimal here).
fn calldata_input<'a>(
    plan: &'a BackrunPlan,
    current_frame: MeasurementContext,
) -> AssembleInput<'a> {
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
        priority_economics: None,
    }
}

fn address_word(address: alloy_primitives::Address) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(address.as_slice());
    word
}

#[test]
fn selector_and_funding_target_tuple_slots_are_pinned() {
    const NEW_SELECTOR: u32 = 0x3b83f272;
    const OLD_SELECTOR: u32 = 0x21def296;

    let selector = u32::from_be_bytes(exec_abi::executeBlinkOfaAtomicCall::SELECTOR);
    assert_eq!(selector, NEW_SELECTOR);
    assert_ne!(selector, OLD_SELECTOR);

    for (protocol, first_target_is_adapter) in [
        (ExactProtocol::UniswapV2, false),
        (ExactProtocol::AerodromeVolatile, false),
        (ExactProtocol::AerodromeStable, false),
        (ExactProtocol::UniswapV3, true),
    ] {
        let victim = keccak256(b"funding-target");
        let plan = backrun_plan([protocol, ExactProtocol::UniswapV2], victim);
        let calldata = encode_executor_calldata(&calldata_input(&plan, matching_frame(&plan)))
            .expect("funding-target calldata");
        let encoded_selector = u32::from_be_bytes(calldata[..4].try_into().expect("selector"));
        assert_eq!(encoded_selector, NEW_SELECTOR);
        assert_ne!(encoded_selector, OLD_SELECTOR);
        assert!(
            !calldata.windows(4).any(|word| word == OLD_SELECTOR.to_be_bytes()),
            "old selector remains in emitted calldata"
        );

        let decoded = exec_abi::executeBlinkOfaAtomicCall::abi_decode(&calldata)
            .expect("decode executor calldata");
        let expected_first = if first_target_is_adapter { ADAPTER } else { plan.route[0].pool };
        assert_eq!(decoded.firstHop.fundingTarget, expected_first);
        assert_eq!(decoded.secondHop.fundingTarget, plan.route[1].pool);

        // Both static SwapHop tuples are flattened in place. fundingTarget is tuple
        // word 6, hence calldata words 6 and 13 after the four-byte selector.
        assert_eq!(&calldata[4 + 6 * 32..4 + 7 * 32], &address_word(expected_first));
        assert_eq!(&calldata[4 + 13 * 32..4 + 14 * 32], &address_word(plan.route[1].pool));
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
    // ★Crucially they ignore even a FRACTIONAL (non-%100) fee: the OutOfRange guard
    // runs first, then the protocol match returns Ok(0) with NO %100 check. A
    // regression that moved the %100 check BEFORE the protocol branch would wrongly
    // reject these — e.g. Slipstream CL's 110 pips (1.1 bps).
    assert_eq!(fee_bps_for_executor(ExactProtocol::UniswapV3, 110), Ok(0));
    assert_eq!(fee_bps_for_executor(ExactProtocol::AerodromeStable, 150), Ok(0));

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

    let mut fractional = backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim);
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

#[test]
fn wrong_frame_victim_fails_the_identity_gate_on_both_paths() {
    // A digest-valid plan for victim B (with a raw tx for B) whose 4 frame fields
    // match a TRUSTED current frame that targets victim A must NOT emit — the
    // current-frame victim is bound IN THE GATE, not merely via the raw↔plan
    // self-consistency check (which only proves the plan agrees with its OWN victim).
    let (victim_raw, victim_b) = victim_with_priority(41);
    let plan = backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim_b);
    let victim_a = keccak256(b"a-different-victim");
    assert_ne!(victim_a, victim_b);
    let wrong_victim_frame = MeasurementContext { victim: victim_a, ..matching_frame(&plan) };

    // Direct calldata path → no-emit.
    assert_eq!(
        encode_executor_calldata(&calldata_input(&plan, wrong_victim_frame)),
        Err(AssembleError::FrameIdentityMismatch),
    );

    // Full-envelope path: the raw↔plan binding PASSES (raw matches plan.victim B),
    // yet the gate still rejects because plan.victim (B) != current_frame.victim (A).
    let reject = AssembleInput {
        plan: &plan,
        current_frame: wrong_victim_frame,
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
        victim_raw_tx: &victim_raw,
        victim_tx_hash: victim_b,
        expected_victim_priority_fee: Some(41),
        priority_economics: None,
    };
    assert_eq!(
        assemble_unsigned_atomic_tx(&reject).unwrap_err(),
        AssembleError::FrameIdentityMismatch,
    );

    // Non-vacuous: the CORRECT current frame (victim B) emits on BOTH paths.
    assert!(encode_executor_calldata(&calldata_input(&plan, matching_frame(&plan))).is_ok());
    let accept = AssembleInput { current_frame: matching_frame(&plan), ..reject };
    assert!(assemble_unsigned_atomic_tx(&accept).is_ok());
}

#[test]
fn self_applying_hop_with_fractional_fee_emits_zero_feebps() {
    // A UniswapV3 first hop carrying a FRACTIONAL sizing fee (110 pips = 1.1 bps,
    // e.g. Slipstream CL) must still emit ABI feeBps 0 — the pool self-applies its
    // fee, so the fraction is canonicalized to 0, never rejected or truncated. The
    // second (UniV2) hop keeps its lossless conversion (3000 pips → 30 bps).
    let victim = keccak256(b"v3-fractional");
    let mut plan = backrun_plan([ExactProtocol::UniswapV3, ExactProtocol::UniswapV2], victim);
    plan.route[0].fee_pips = 110;
    finalize_plan_digest(&mut plan);
    let calldata = encode_executor_calldata(&calldata_input(&plan, matching_frame(&plan)))
        .expect("V3 fractional-fee calldata must emit");
    let decoded = exec_abi::executeBlinkOfaAtomicCall::abi_decode(&calldata)
        .expect("decode executor calldata");
    assert_eq!(decoded.firstHop.feeBps, U24::ZERO, "V3 fractional fee did not canonicalize to 0");
    assert_eq!(decoded.secondHop.feeBps, U24::from(30u32), "UniV2 hop feeBps not 30");
}

#[test]
fn arm_validated_path_requires_fresh_gas_and_l1_fee_authority() {
    let (victim_raw, victim_hash) = victim_with_priority(37);
    let plan = backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim_hash);
    let mut input = calldata_input(&plan, matching_frame(&plan));
    input.victim_raw_tx = &victim_raw;
    input.victim_tx_hash = victim_hash;
    input.expected_victim_priority_fee = Some(37);

    assert_eq!(assemble_validated(&input).unwrap_err(), AssembleError::PriorityEconomicsRejected);

    input.priority_economics = Some(PriorityEconomicsAuthority::new(
        U256::from(1),
        U256::from(1),
        U256::from(1),
        plan.block_number + 1,
    ));
    assert_eq!(assemble_validated(&input).unwrap_err(), AssembleError::PriorityEconomicsRejected);

    input.priority_economics = Some(PriorityEconomicsAuthority::new(
        U256::from(1),
        U256::from(1),
        U256::from(1),
        plan.block_number,
    ));
    assert!(assemble_validated(&input).is_ok());

    let source = include_str!("../src/assembler.rs");
    let validated =
        source.split("pub fn assemble_validated").nth(1).expect("validated assembly sink");
    assert!(
        validated.find("let decision = evaluate").unwrap()
            < validated.find("Ok(LegacyValidatedUnsignedAtomicTx").unwrap(),
        "economics gate must precede the arm witness consumed by signing"
    );
}
