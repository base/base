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
/// (see the crate docs). The revm-backed `gas_primitives_match_evm_reference`
/// test in `base-execution-eip8130` is a drift tripwire that pins the EVM
/// primitives to revm's canonical constants, so an upstream repricing is
/// surfaced and re-decided deliberately rather than tracked silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Eip8130GasSchedule;

impl Eip8130GasSchedule {
    // ── EIP-2929 storage access ──────────────────────────────────────────────
    /// Cold `SLOAD` (first access to a slot in the transaction).
    pub const COLD_SLOAD: u64 = 2_100;
    /// Warm `SLOAD` (repeat access to an already-touched slot).
    pub const WARM_SLOAD: u64 = 100;
    /// `SSTORE` of a zero slot to a non-zero value.
    pub const SSTORE_SET: u64 = 20_000;
    /// `SSTORE` of an already non-zero slot to another non-zero value.
    pub const SSTORE_RESET: u64 = 2_900;
    /// `SSTORE` to a slot already modified earlier in the same transaction (its
    /// EIP-2200 `original != current` "dirty" case). Repricing a dirty slot only
    /// adjusts the gas meter, so it costs a warm storage read.
    pub const SSTORE_DIRTY: u64 = Self::WARM_SLOAD;

    // ── EIP-2028 data availability ───────────────────────────────────────────
    /// Cost of a zero byte of serialized transaction data.
    pub const TX_DATA_ZERO_BYTE: u64 = 4;
    /// Cost of a non-zero byte of serialized transaction data.
    pub const TX_DATA_NONZERO_BYTE: u64 = 16;

    // ── EIP-8130 table values ────────────────────────────────────────────────
    /// Base intrinsic cost for any AA transaction (`AA_BASE_COST`).
    pub const AA_BASE_COST: u64 = Eip8130Constants::EIP8130_BASE_COST;
    /// `nonce_key_cost` for nonce-free (`NONCE_KEY_MAX`) transactions: 13,000 gas
    /// for the enshrined ring-buffer replay state, composed of 2 cold SLOADs, 1
    /// warm SLOAD, and 3 warm SSTORE resets. The ring pointer's SLOAD/SSTORE are
    /// amortized across the block, so EIP-8130 prices this as a fixed composite
    /// rather than metering the individual accesses (the raw per-op cost, e.g. an
    /// `SSTORE_SET` per insert, is far higher but amortized by the ring reclaiming
    /// a slot on each write).
    pub const NONCE_FREE_COST: u64 =
        2 * Self::COLD_SLOAD + Self::WARM_SLOAD + 3 * Self::SSTORE_RESET;
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
    //
    // `ACTOR_SLOT_SET_COST`, `ACCOUNT_STATE_SET_COST`, and
    // `CONFIG_CHANGE_STATE_COST` all currently equal one fresh-slot write (cold
    // SLOAD + SSTORE set = 22,100). They are kept as three distinct named
    // constants — rather than a single shared `FRESH_SLOT_SET_COST` — because
    // they price semantically different accesses (an actor/policy slot, the
    // packed-state bootstrap, and a config-change sequence bump) that may be
    // repriced independently. `CONFIG_CHANGE_STATE_COST` aliasing
    // `ACCOUNT_STATE_SET_COST` documents that today they share the conservative
    // zero-to-nonzero bound; a future reset-vs-set split would touch only the
    // relevant name.
    /// Writing a fresh actor slot (`actor_config`, or a policy slot) — cold SLOAD
    /// + SSTORE set.
    pub const ACTOR_SLOT_SET_COST: u64 = Self::COLD_SLOAD + Self::SSTORE_SET;
    /// Overwriting an already-set actor slot (e.g. a revoke) — cold SLOAD +
    /// SSTORE reset.
    pub const ACTOR_SLOT_RESET_COST: u64 = Self::COLD_SLOAD + Self::SSTORE_RESET;
    /// Touching a cold slot with a zero-to-zero SSTORE: cold access plus the warm
    /// no-op write cost.
    pub const COLD_SLOT_NOOP_COST: u64 = Self::COLD_SLOAD + Self::WARM_SLOAD;
    /// Touching both policy slots with zero-to-zero clears for an ungated actor.
    pub const POLICY_SLOTS_NOOP_COST: u64 = Self::COLD_SLOT_NOOP_COST * 2;
    /// Initializing the packed account-state slot. Create entries always perform
    /// this write, and a config change may be the first state established for an
    /// otherwise untouched EOA.
    pub const ACCOUNT_STATE_SET_COST: u64 = Self::COLD_SLOAD + Self::SSTORE_SET;
    /// Conservative cost for the **first** access to the packed account-state
    /// slot in a transaction (a create bootstrap or the first config change). The
    /// same access also supplies the lock status and both sequence channels, so
    /// those checks must not be charged as extra reads.
    ///
    /// A previously initialized account would pay the lower reset cost, but the
    /// transaction body cannot prove the pre-state. Pricing the possible
    /// zero-to-nonzero transition prevents first-change undercharging.
    pub const CONFIG_CHANGE_STATE_COST: u64 = Self::ACCOUNT_STATE_SET_COST;
    /// Cost for a **subsequent** config change to the same account's packed
    /// account-state slot within one transaction. A create or an earlier config
    /// change already made this slot warm *and modified it earlier in the same
    /// transaction*, so the further sequence bump is a warm SLOAD plus a **dirty**
    /// `SSTORE` (EIP-2200 `original != current`) — not another cold zero-to-nonzero
    /// write, nor even a reset (which applies only to the first, `original ==
    /// current`, modification). All config changes in a transaction target the
    /// same (`sender`) account, so every change after the first is priced here.
    /// This is body-derivable (the change's position is known), so estimation and
    /// execution price it identically.
    pub const CONFIG_CHANGE_STATE_COST_SUBSEQUENT: u64 = Self::WARM_SLOAD + Self::SSTORE_DIRTY;
    /// Marginal cost of an `IncrementLocalEpoch` op. The op rewrites the packed
    /// account-state slot (`local_epoch ‖ local_sequence`) that the config
    /// change's own channel-sequence advance already touched and modified earlier
    /// in the transaction, so the epoch bump is a warm **dirty** `SSTORE`
    /// (EIP-2200 `original != current`) with no extra SLOAD — the ~100-gas
    /// already-warm write the contract notes for the trailing increment. In the
    /// rarer unsequenced-and-initialized case the advance performs no write, but
    /// the first change's `CONFIG_CHANGE_STATE_COST` conservatively charged a full
    /// zero-to-nonzero set for that slot, so adding this marginal cost still never
    /// undercharges.
    pub const INCREMENT_LOCAL_EPOCH_COST: u64 = Self::SSTORE_DIRTY;
    /// Worst-case revoke cost for the actor config and its two policy slots.
    ///
    /// Policy slots are cleared on every revoke. Charging all three as resets is
    /// conservative for ungated actors (whose policy slots are already zero) and
    /// exact for a policy-bearing actor.
    pub const ACTOR_REVOKE_COST: u64 = Self::ACTOR_SLOT_RESET_COST * 3;
    /// Gas over-charged by [`Self::ACTOR_REVOKE_COST`] for a single revoke slot
    /// that was actually an empty zero-to-zero touch rather than an `SSTORE` reset:
    /// the reset-vs-cold-noop delta ([`Self::ACTOR_SLOT_RESET_COST`] −
    /// [`Self::COLD_SLOT_NOOP_COST`]). A revoke of the account's **inline** secp256k1
    /// self key has an empty `actor_config` slot always, and empty policy slots when
    /// the self was ungated: 3 empty slots ungated, 1 when policy-gated (its two
    /// policy slots are real resets). Execution resolves how many of a revoke's
    /// three slots were empty and the intrinsic layer subtracts this discount per
    /// such slot; a non-resolved (zero) count leaves the conservative reset price in
    /// place, so this can only reduce, never under-price, the charge.
    pub const COLD_SLOT_RESET_DISCOUNT: u64 =
        Self::ACTOR_SLOT_RESET_COST - Self::COLD_SLOT_NOOP_COST;

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
