//! Shared test fixtures for the `mev-trader-submit` integration tests.
//!
//! Test-only: this module and everything it references is compiled solely for the
//! crate's tests and is never linked into any node binary.
#![cfg(feature = "phase-b")]
// Test fixtures are scoped to each test binary; `pub` here is intra-binary only.
#![allow(dead_code, unreachable_pub)]

pub mod bytecode;

use alloy_consensus::TxEip1559;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, address, keccak256};
use alloy_rpc_types_engine::PayloadId;
use base_mev_trader::{
    BackrunHop, BackrunPlan, BackrunPlanDigest, ExactProtocol, MeasurementContext,
    MeasurementEncoder,
};
use mev_trader_submit::signer::sign_ephemeral_atomic_tx;

/// Base wrapped native (WETH) — the closed-loop start/end token.
pub const WETH: Address = address!("4200000000000000000000000000000000000006");
/// Deterministic executor address used by the byte-parity golden.
pub const EXECUTOR: Address = address!("2000000000000000000000000000000000000002");
/// Deterministic adapter address used by the byte-parity golden.
pub const ADAPTER: Address = address!("00000000000000000000000000000000000000a1");
/// Deterministic intermediate token used by the byte-parity golden.
pub const TOKEN: Address = address!("00000000000000000000000000000000000000c0");
/// Deterministic first-hop pool used by the byte-parity golden.
pub const POOL1: Address = address!("00000000000000000000000000000000000000f1");
/// Deterministic second-hop pool used by the byte-parity golden.
pub const POOL2: Address = address!("00000000000000000000000000000000000000f2");

/// 1 WETH principal.
pub const AMOUNT_IN: u128 = 1_000_000_000_000_000_000;
/// 1.01 WETH sized output floor (`minFinalAmount`).
pub const MIN_FINAL: u128 = 1_010_000_000_000_000_000;
/// First-hop minimum output used by the golden.
pub const FIRST_MIN_OUT: u128 = 1_234_567_890;
/// The canonical sizing fee in pips carried by both golden hops (0.30% → feeBps 30).
pub const FEE_PIPS: u32 = 3_000;

/// Build a two-hop WETH closed-loop [`BackrunPlan`] with the given hop protocols and
/// a valid self-excluding digest (R8: hops carry `FEE_PIPS`, bound into the digest).
pub fn backrun_plan(protocols: [ExactProtocol; 2], victim: B256) -> BackrunPlan {
    let mut plan = BackrunPlan {
        parent_hash: B256::ZERO,
        block_number: 0,
        predecessor_index: 0,
        payload_id: PayloadId::new([0u8; 8]),
        victim,
        route: [
            BackrunHop {
                pool: POOL1,
                protocol: protocols[0],
                token_in: WETH,
                token_out: TOKEN,
                fee_pips: FEE_PIPS,
            },
            BackrunHop {
                pool: POOL2,
                protocol: protocols[1],
                token_in: TOKEN,
                token_out: WETH,
                fee_pips: FEE_PIPS,
            },
        ],
        amount_in: U256::from(AMOUNT_IN),
        amount_out: U256::from(MIN_FINAL),
        gross_profit: U256::from(MIN_FINAL - AMOUNT_IN),
        digest: BackrunPlanDigest(B256::ZERO),
    };
    finalize_plan_digest(&mut plan);
    plan
}

/// Recompute and store the self-excluding digest after mutating a plan's fields, so
/// the digest self-check in the assembler still passes for the mutated plan.
pub fn finalize_plan_digest(plan: &mut BackrunPlan) {
    plan.digest = MeasurementEncoder::digest(plan).expect("valid measurement plan digest");
}

/// The trusted current-frame identity that EXACTLY matches `plan`'s frame — the
/// happy-path value for [`mev_trader_submit::assembler::AssembleInput::current_frame`].
pub fn matching_frame(plan: &BackrunPlan) -> MeasurementContext {
    MeasurementContext {
        parent_hash: plan.parent_hash,
        block_number: plan.block_number,
        predecessor_index: plan.predecessor_index,
        payload_id: plan.payload_id,
        victim: plan.victim,
    }
}

/// Build a parseable signed EIP-1559 victim envelope with the given priority fee,
/// returning `(raw_bytes, tx_hash)`. The victim is signed with a throwaway
/// ephemeral key only to obtain a well-formed envelope; its signer is irrelevant.
pub fn victim_with_priority(priority_fee: u128) -> (Vec<u8>, B256) {
    let unsigned = TxEip1559 {
        chain_id: 8453,
        nonce: 1,
        gas_limit: 100_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: priority_fee,
        to: TxKind::Call(POOL1),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let signed = sign_ephemeral_atomic_tx(&unsigned).expect("victim envelope");
    let hash = keccak256(&signed.raw_backrun);
    (signed.raw_backrun, hash)
}
