//! Gas schedule for EIP-8130 intrinsic-gas accounting.

use alloy_primitives::Address;
use base_common_consensus::{Eip8130Constants, Eip8130Contracts};

/// Per-component gas costs for EIP-8130 intrinsic gas.
///
/// This schedule is a **recommendation at the current point in time**, not a
/// fixed protocol constant. EIP-8130 lets each chain decide how it prices
/// intrinsic gas and (enshrined) authenticator execution, so a chain MAY adopt a
/// different schedule; these are the values Base uses today.
///
/// The storage primitives are the EIP-2929 access costs and the data-byte costs
/// are EIP-2028; together they reproduce the EIP-8130 `nonce_key_cost` table
/// (cold SLOAD + SSTORE set = 22,100; cold SLOAD + warm SSTORE reset = 5,000).
/// The authenticator execution costs are the chain-policy values for the
/// enshrined canonical authenticators, set to the EVM precompile costs Base uses
/// (see the crate docs). The `gas_primitives_match_evm_reference` test is a
/// drift tripwire that pins the EVM primitives to revm's canonical constants, so
/// an upstream repricing is surfaced and re-decided deliberately rather than
/// tracked silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Eip8130GasSchedule;

impl Eip8130GasSchedule {
    // ── EIP-2929 storage access ──────────────────────────────────────────────
    /// Cold `SLOAD` (first access to a slot in the transaction).
    pub const COLD_SLOAD: u64 = 2_100;
    /// `SSTORE` of a zero slot to a non-zero value.
    pub const SSTORE_SET: u64 = 20_000;
    /// `SSTORE` of an already non-zero slot to another non-zero value.
    pub const SSTORE_RESET: u64 = 2_900;

    // ── EIP-2028 data availability ───────────────────────────────────────────
    /// Cost of a zero byte of serialized transaction data.
    pub const TX_DATA_ZERO_BYTE: u64 = 4;
    /// Cost of a non-zero byte of serialized transaction data.
    pub const TX_DATA_NONZERO_BYTE: u64 = 16;

    // ── EIP-8130 table values ────────────────────────────────────────────────
    /// Base intrinsic cost for any AA transaction (`AA_BASE_COST`).
    pub const AA_BASE_COST: u64 = Eip8130Constants::EIP8130_BASE_COST;
    /// `nonce_key_cost` for nonce-free (`NONCE_KEY_MAX`) transactions. A flat,
    /// amortized charge for the enshrined expiring-nonce circular-buffer replay
    /// set: a replay-check SLOAD, the ring pointer + ring-slot reads, reclaiming
    /// one expired entry, recording the new entry, and advancing the pointer. The
    /// raw per-op SSTORE cost is far higher (an `SSTORE_SET` per insert) but is
    /// amortized by the ring reclaiming a slot on each write, so EIP-8130 prices
    /// it as a fixed value rather than metering the individual accesses.
    pub const NONCE_FREE_COST: u64 = 14_000;
    /// `nonce_key_cost` for the first use of a sequence nonce key (cold SLOAD +
    /// SSTORE set).
    pub const NONCE_KEY_FIRST_USE_COST: u64 = Self::COLD_SLOAD + Self::SSTORE_SET;
    /// `nonce_key_cost` for a previously-used sequence nonce key (cold SLOAD +
    /// SSTORE reset).
    pub const NONCE_KEY_EXISTING_COST: u64 = Self::COLD_SLOAD + Self::SSTORE_RESET;
    /// `bytecode_cost` deployment base for a create entry.
    pub const CREATE_BASE_COST: u64 = 32_000;
    /// Code-deposit cost per byte of deployed account bytecode.
    pub const CODE_DEPOSIT_PER_BYTE: u64 = 200;
    /// Compile-time guard that `DELEGATION_INDICATOR_SIZE` fits in `u64`, so the
    /// `as u64` cast in [`Self::DELEGATION_DEPOSIT_COST`] can never truncate
    /// (it is `23` today). Keeps the cast consistent with the
    /// `u64::try_from(..).unwrap_or(u64::MAX)` discipline used for runtime casts.
    const _DELEGATION_INDICATOR_FITS_U64: () =
        assert!(Eip8130Constants::DELEGATION_INDICATOR_SIZE <= u64::MAX as usize);
    /// Delegation-indicator deposit: `200 × 23` for the `0xef0100 || address`
    /// indicator (`auto_delegation_cost` and per delegation entry).
    pub const DELEGATION_DEPOSIT_COST: u64 =
        Self::CODE_DEPOSIT_PER_BYTE * Eip8130Constants::DELEGATION_INDICATOR_SIZE as u64;

    // ── Config-change actor slot writes ──────────────────────────────────────
    /// Writing a fresh actor slot (`actor_config`, or a policy slot) — cold SLOAD
    /// + SSTORE set.
    pub const ACTOR_SLOT_SET_COST: u64 = Self::COLD_SLOAD + Self::SSTORE_SET;
    /// Overwriting an already-set actor slot (e.g. a revoke) — cold SLOAD +
    /// SSTORE reset.
    pub const ACTOR_SLOT_RESET_COST: u64 = Self::COLD_SLOAD + Self::SSTORE_RESET;
    /// Worst-case extra cost for a config change targeting the account's own
    /// secp256k1 self-actor. The self key's config lives inline in the
    /// account-state slot, so authorizing or revoking it mutates that slot *and*
    /// touches the mutually-exclusive `actor_config(self)` home — a second
    /// storage home a non-self actor change never writes. Priced at one fresh
    /// slot write (cold SLOAD + SSTORE set) as a safe upper bound over the
    /// actual set/reset/clear mix.
    pub const SELF_ACTOR_DUAL_HOME_COST: u64 = Self::ACTOR_SLOT_SET_COST;

    // ── Enshrined authenticator execution gas (chain policy) ─────────────────
    /// secp256k1 (`K1_AUTHENTICATOR` sentinel / EOA path) execution gas — the
    /// `ECRECOVER` precompile cost.
    pub const AUTH_EXEC_K1: u64 = 3_000;
    /// P-256 authenticator execution gas — the EIP-7951 `P256VERIFY` precompile
    /// cost.
    pub const AUTH_EXEC_P256: u64 = 6_900;
    /// `WebAuthn` authenticator execution gas — P-256 verify plus SHA-256 and
    /// `clientDataJSON` handling, charged at the same enshrined cost as raw
    /// P-256.
    pub const AUTH_EXEC_WEBAUTHN: u64 = 6_900;
    /// Extra execution overhead for the delegate authenticator: the cold
    /// `actor_config` SLOAD on the delegate account, on top of the nested
    /// authenticator's own execution gas.
    pub const AUTH_EXEC_DELEGATE_OVERHEAD: u64 = Self::COLD_SLOAD;

    /// Execution gas for a leaf (non-delegate) enshrined authenticator, or `None`
    /// for a non-canonical address (such a transaction is rejected by dispatch
    /// before its gas is charged).
    #[must_use]
    pub fn leaf_auth_exec_gas(authenticator: Address) -> Option<u64> {
        if authenticator == Eip8130Constants::K1_AUTHENTICATOR {
            Some(Self::AUTH_EXEC_K1)
        } else if authenticator == Eip8130Contracts::P256_AUTHENTICATOR {
            Some(Self::AUTH_EXEC_P256)
        } else if authenticator == Eip8130Contracts::WEBAUTHN_AUTHENTICATOR {
            Some(Self::AUTH_EXEC_WEBAUTHN)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use revm::interpreter::gas;

    use super::*;

    /// The schedule is a recommendation built on the current EIP-2929/EIP-2028
    /// EVM primitives. This is a drift tripwire, not an invariant: if revm
    /// reprices a primitive (e.g. via a hardfork), this fails so the schedule
    /// (and the EIP) can be re-decided deliberately rather than the change being
    /// adopted silently. It also documents the (non-obvious) name mapping.
    #[test]
    fn gas_primitives_match_evm_reference() {
        assert_eq!(Eip8130GasSchedule::COLD_SLOAD, gas::COLD_SLOAD_COST);
        assert_eq!(Eip8130GasSchedule::SSTORE_SET, gas::SSTORE_SET);
        // revm's `SSTORE_RESET` (5,000) bundles the cold SLOAD; the warm-only
        // reset component is `WARM_SSTORE_RESET` (2,900), which the schedule's
        // composites add on top of `COLD_SLOAD` separately.
        assert_eq!(Eip8130GasSchedule::SSTORE_RESET, gas::WARM_SSTORE_RESET);
        // A zero byte is one standard calldata token; a non-zero byte is the
        // EIP-2028 (Istanbul) cost, not the EIP-7623 floor token.
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
    }
}
