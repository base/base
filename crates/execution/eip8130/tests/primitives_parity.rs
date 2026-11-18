//! Parity guards for the EIP-8130 gas primitives, which now live in the
//! engine-neutral `base-common-eip8130` crate and are re-exported here.
//!
//! These checks are kept in `base-execution-eip8130` — rather than beside the
//! primitives — because each pins the revm-free schedule/metering against
//! something only the revm execution path has: revm's canonical gas constants
//! and calldata token counter.

use alloy_primitives::{Address, Bytes};
use base_common_consensus::{Eip8130Constants, Eip8130Signed, TxEip8130};
use base_execution_eip8130::{Eip8130GasSchedule, IntrinsicGas};
use revm::{context_interface::cfg::gas::get_tokens_in_calldata_istanbul, interpreter::gas};

/// The schedule is a recommendation built on the current EIP-2929/EIP-2028 EVM
/// primitives. This is a drift tripwire, not an invariant: if revm reprices a
/// primitive (e.g. via a hardfork), this fails so the schedule (and the EIP) can
/// be re-decided deliberately rather than the change being adopted silently. It
/// also documents the (non-obvious) name mapping.
#[test]
fn gas_primitives_match_evm_reference() {
    assert_eq!(Eip8130GasSchedule::COLD_SLOAD, gas::COLD_SLOAD_COST);
    assert_eq!(Eip8130GasSchedule::WARM_SLOAD, gas::WARM_STORAGE_READ_COST);
    assert_eq!(Eip8130GasSchedule::SSTORE_SET, gas::SSTORE_SET);
    assert_eq!(Eip8130GasSchedule::COLD_ACCOUNT_ACCESS, gas::COLD_ACCOUNT_ACCESS_COST);
    // revm's `SSTORE_RESET` (5,000) bundles the cold SLOAD; the warm-only reset
    // component is `WARM_SSTORE_RESET` (2,900), which the schedule's composites
    // add on top of `COLD_SLOAD` separately.
    assert_eq!(Eip8130GasSchedule::SSTORE_RESET, gas::WARM_SSTORE_RESET);
    // A zero byte is one standard calldata token; a non-zero byte is the EIP-2028
    // (Istanbul) cost, not the EIP-7623 floor token.
    assert_eq!(Eip8130GasSchedule::TX_DATA_ZERO_BYTE, gas::STANDARD_TOKEN_COST);
    assert_eq!(Eip8130GasSchedule::TX_DATA_NONZERO_BYTE, gas::NON_ZERO_BYTE_DATA_COST_ISTANBUL);
    // The EIP-7623 per-token floor an 8130 transaction pays over its serialized
    // payload, pinned to revm's `TOTAL_COST_FLOOR_PER_TOKEN`.
    assert_eq!(Eip8130GasSchedule::TX_TOTAL_COST_FLOOR_PER_TOKEN, gas::TOTAL_COST_FLOOR_PER_TOKEN);
    assert_eq!(Eip8130GasSchedule::CODE_DEPOSIT_PER_BYTE, gas::CODEDEPOSIT);

    // The EIP-8130 `nonce_key_cost` composites these primitives reproduce.
    assert_eq!(
        Eip8130GasSchedule::NONCE_KEY_FIRST_USE_COST,
        gas::COLD_SLOAD_COST + gas::SSTORE_SET
    );
    assert_eq!(
        Eip8130GasSchedule::NONCE_KEY_EXISTING_COST,
        gas::COLD_SLOAD_COST + gas::WARM_SSTORE_RESET
    );
    // Nonce-free ring-buffer cost: 2 cold SLOADs + 1 warm SLOAD + 3 warm SSTORE
    // resets = 13,000 gas.
    assert_eq!(
        Eip8130GasSchedule::NONCE_FREE_COST,
        2 * gas::COLD_SLOAD_COST + gas::WARM_STORAGE_READ_COST + 3 * gas::WARM_SSTORE_RESET
    );
    assert_eq!(Eip8130GasSchedule::NONCE_FREE_COST, 13_000);
}

/// `payload` and `payload_floor` count tokens by hand (one per zero byte, four
/// per non-zero byte). This pins that count to revm's calldata token counter
/// over the same metered bytes, so a change to either formula fails here.
#[test]
fn payload_token_count_matches_evm_reference() {
    let mut metadata = vec![0u8; 40];
    metadata.extend((1..=200u8).collect::<Vec<_>>());
    let tx = TxEip8130 {
        chain_id: 8453,
        gas_limit: 1_000_000,
        max_fee_per_gas: 1,
        metadata: Bytes::from(metadata),
        payer: Some(Address::repeat_byte(0x22)),
        ..Default::default()
    };
    let mut payer_auth = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
    payer_auth.extend([0xabu8; 65]);
    let signed = Eip8130Signed::new(tx, Bytes::from(vec![0xcdu8; 65]), Bytes::from(payer_auth));

    let gas = IntrinsicGas::compute(&signed, Address::repeat_byte(0x11));
    let mut metered = Vec::new();
    signed.tx().encode_metered_payload(&mut metered);
    let tokens = get_tokens_in_calldata_istanbul(&metered);
    assert_eq!(gas.payload, tokens * gas::STANDARD_TOKEN_COST);
    assert_eq!(gas.payload_floor, tokens * gas::TOTAL_COST_FLOOR_PER_TOKEN);
}
