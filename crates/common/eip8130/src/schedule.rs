//! Gas schedule for EIP-8130 intrinsic-gas accounting.

use alloy_primitives::Address;
use base_common_consensus::Eip8130Constants;

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
/// The authenticator execution cost is the chain-policy value for the enshrined
/// secp256k1 authenticator, set to the EVM precompile cost Base uses. The
/// revm-backed `gas_primitives_match_evm_reference` test in
/// `base-execution-eip8130` is a drift tripwire that pins the EVM primitives to
/// revm's canonical constants, so an upstream repricing is surfaced and
/// re-decided deliberately rather than tracked silently.
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

    // ── EIP-2028 data availability ───────────────────────────────────────────
    /// Cost of a zero byte of serialized transaction data.
    pub const TX_DATA_ZERO_BYTE: u64 = 4;
    /// Cost of a non-zero byte of serialized transaction data.
    pub const TX_DATA_NONZERO_BYTE: u64 = 16;

    // ── EIP-7623 calldata floor ──────────────────────────────────────────────
    /// `TOTAL_COST_FLOOR_PER_TOKEN`: the per-token gas an EIP-7623 transaction
    /// pays for data availability under the floor branch. An 8130 transaction has
    /// no single `data` field, so the floor is evaluated over the serialized
    /// transaction (one token per zero byte, four per non-zero byte). A data-heavy
    /// transaction whose execution is cheap pays `payload_tokens * this` instead
    /// of the standard `payload_tokens * TX_DATA_ZERO_BYTE`, so it cannot post data
    /// availability more cheaply than a standard EIP-7623 transaction on the same
    /// chain. Pinned to revm's `TOTAL_COST_FLOOR_PER_TOKEN` by the
    /// `gas_primitives_match_evm_reference` drift tripwire.
    pub const TX_TOTAL_COST_FLOOR_PER_TOKEN: u64 = 10;

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
    /// Code-deposit cost per byte of deployed account bytecode.
    pub const CODE_DEPOSIT_PER_BYTE: u64 = 200;
    /// Compile-time guard that `DELEGATION_INDICATOR_SIZE` fits in `u64`, so the
    /// `as u64` cast in [`Self::DELEGATION_DEPOSIT_COST`] can never truncate
    /// (it is `23` today). Keeps the cast consistent with the
    /// `u64::try_from(..).unwrap_or(u64::MAX)` discipline used for runtime casts.
    const _DELEGATION_INDICATOR_FITS_U64: () =
        assert!(Eip8130Constants::DELEGATION_INDICATOR_SIZE <= u64::MAX as usize);
    /// Delegation-indicator deposit: `200 × 23` for the `0xef0100 || address`
    /// indicator, charged per delegation entry.
    pub const DELEGATION_DEPOSIT_COST: u64 =
        Self::CODE_DEPOSIT_PER_BYTE * Eip8130Constants::DELEGATION_INDICATOR_SIZE as u64;
    /// `TX_VALUE_COST`: per call with `value > 0` and `to != sender`, covering
    /// the recipient balance write and the transfer log. Charged statically
    /// because `to` and `value` are signed fields.
    pub const TX_VALUE_COST: u64 = 6_000;
    /// Account-creation charge for a value-bearing call to an account that does
    /// not exist, charged at dispatch because existence is only known then.
    pub const NEW_ACCOUNT_COST: u64 = 25_000;
    /// Upper bound on `payer_auth_cost`, which is metered outside `gas_limit`.
    pub const MAX_AUTHENTICATION_GAS: u64 = 100_000;

    // ── Enshrined authenticator execution gas (chain policy) ─────────────────
    /// secp256k1 (`K1_AUTHENTICATOR` sentinel / EOA path) execution gas — the
    /// `ECRECOVER` precompile cost.
    pub const AUTH_EXEC_K1: u64 = 3_000;

    /// Execution gas for a leaf enshrined authenticator, or `None` for a
    /// non-canonical address. On the launch wire the only enshrined authenticator
    /// is the native secp256k1 sentinel.
    #[must_use]
    pub fn leaf_auth_exec_gas(authenticator: Address) -> Option<u64> {
        (authenticator == Eip8130Constants::K1_AUTHENTICATOR).then_some(Self::AUTH_EXEC_K1)
    }
}
