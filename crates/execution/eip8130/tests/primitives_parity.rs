//! Parity guards for the EIP-8130 gas primitives, which now live in the
//! engine-neutral `base-common-eip8130` crate and are re-exported here.
//!
//! These two checks are kept in `base-execution-eip8130` — rather than beside the
//! primitives — because each pins the revm-free schedule/metering against
//! something only the revm execution path has: revm's canonical gas constants,
//! and the validating account-change decoder ([`AccountChangeApplier`]).

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::SolValue;
use base_common_consensus::Eip8130Constants;
use base_execution_eip8130::{AccountChangeApplier, Eip8130GasSchedule, IntrinsicGas};
use revm::interpreter::gas;

alloy_sol_types::sol! {
    // Mirror of the contract's `ActorConfig` authorize payload, used only to pin
    // the byte offsets `authorize_attaches_policy` reads.
    struct ActorConfigAbi {
        address authenticator;
        uint48 expiry;
        uint16 scope;
    }
}

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
    // revm's `SSTORE_RESET` (5,000) bundles the cold SLOAD; the warm-only reset
    // component is `WARM_SSTORE_RESET` (2,900), which the schedule's composites
    // add on top of `COLD_SLOAD` separately.
    assert_eq!(Eip8130GasSchedule::SSTORE_RESET, gas::WARM_SSTORE_RESET);
    // A zero byte is one standard calldata token; a non-zero byte is the EIP-2028
    // (Istanbul) cost, not the EIP-7623 floor token.
    assert_eq!(Eip8130GasSchedule::TX_DATA_ZERO_BYTE, gas::STANDARD_TOKEN_COST);
    assert_eq!(Eip8130GasSchedule::TX_DATA_NONZERO_BYTE, gas::NON_ZERO_BYTE_DATA_COST_ISTANBUL);
    assert_eq!(Eip8130GasSchedule::CODE_DEPOSIT_PER_BYTE, gas::CODEDEPOSIT);
    assert_eq!(Eip8130GasSchedule::CREATE_BASE_COST, gas::CREATE);

    // The EIP-8130 `nonce_key_cost` composites these primitives reproduce.
    assert_eq!(
        Eip8130GasSchedule::NONCE_KEY_FIRST_USE_COST,
        gas::COLD_SLOAD_COST + gas::SSTORE_SET
    );
    assert_eq!(
        Eip8130GasSchedule::NONCE_KEY_EXISTING_COST,
        gas::COLD_SLOAD_COST + gas::WARM_SSTORE_RESET
    );
    assert_eq!(
        Eip8130GasSchedule::COLD_SLOT_NOOP_COST,
        gas::COLD_SLOAD_COST + gas::WARM_STORAGE_READ_COST
    );
    // A subsequent same-account state bump is a warm SLOAD + a dirty SSTORE (the
    // slot was already modified earlier in this transaction).
    assert_eq!(
        Eip8130GasSchedule::CONFIG_CHANGE_STATE_COST_SUBSEQUENT,
        gas::WARM_STORAGE_READ_COST + gas::WARM_STORAGE_READ_COST
    );
    // An empty revoke slot is priced down from a reset to a cold zero-to-zero
    // touch; the per-slot discount is the difference.
    assert_eq!(
        Eip8130GasSchedule::COLD_SLOT_RESET_DISCOUNT,
        (gas::COLD_SLOAD_COST + gas::WARM_SSTORE_RESET)
            - (gas::COLD_SLOAD_COST + gas::WARM_STORAGE_READ_COST)
    );
    // Nonce-free ring-buffer cost: 2 cold SLOADs + 1 warm SLOAD + 3 warm SSTORE
    // resets = 13,000 gas.
    assert_eq!(
        Eip8130GasSchedule::NONCE_FREE_COST,
        2 * gas::COLD_SLOAD_COST + gas::WARM_STORAGE_READ_COST + 3 * gas::WARM_SSTORE_RESET
    );
    assert_eq!(Eip8130GasSchedule::NONCE_FREE_COST, 13_000);
}

#[test]
fn authorize_attaches_policy_agrees_with_apply_decode() {
    // Cross-crate coupling guard: whenever the apply-side (validating) decoder
    // accepts a payload, intrinsic metering must agree with the *decoded*
    // policyData on whether policy is attached. `authorize_attaches_policy`
    // follows the same ABI offset pointer the decoder does, so it cannot
    // under-meter (report "no policy" while a 52-byte policy is written) — the
    // exact drift a naive "canonical offset only" check would risk if the decoder
    // were ever relaxed.
    let build = |policy: &[u8]| -> Vec<u8> {
        (
            B256::repeat_byte(0x11),
            ActorConfigAbi {
                authenticator: Address::ZERO,
                scope: Eip8130Constants::SCOPE_OPERATOR,
                expiry: alloy_primitives::Uint::ZERO,
            },
            Bytes::from(policy.to_vec()),
        )
            .abi_encode_params()
    };
    for policy in [vec![], vec![0u8; Eip8130Constants::POLICY_DATA_LEN]] {
        let payload = build(&policy);
        let (_, _, decoded) = AccountChangeApplier::decode_authorize(&payload).unwrap();
        assert_eq!(
            IntrinsicGas::authorize_attaches_policy(&payload),
            AccountChangeApplier::policy_attached(&decoded),
        );
    }

    // Hand-corrupt the offset pointer to a non-canonical (but in-range) value. The
    // validating decoder follows the pointer (it does *not* reject this), landing
    // on an all-zero word ⇒ length 0 ⇒ no policy; the metering follows the same
    // pointer and agrees. A canonical-offset-only check would have mismatched.
    let mut corrupted = build(&[0u8; Eip8130Constants::POLICY_DATA_LEN]);
    corrupted[128..160].copy_from_slice(&U256::from(192).to_be_bytes::<32>());
    let (_, _, decoded) = AccountChangeApplier::decode_authorize(&corrupted).unwrap();
    assert_eq!(
        IntrinsicGas::authorize_attaches_policy(&corrupted),
        AccountChangeApplier::policy_attached(&decoded),
    );
    assert!(!AccountChangeApplier::policy_attached(&decoded));
}
