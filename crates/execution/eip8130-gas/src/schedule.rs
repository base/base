//! Gas schedule for EIP-8130 intrinsic-gas accounting.

use alloy_primitives::Address;
use base_common_consensus::{Eip8130Constants, Eip8130Contracts};

/// Per-component gas costs for EIP-8130 intrinsic gas.
///
/// The storage primitives are the EIP-2929 access costs and the data-byte costs
/// are EIP-2028; together they reproduce the EIP-8130 `nonce_key_cost` table
/// exactly (cold SLOAD + SSTORE set = 22,100; cold SLOAD + SSTORE reset =
/// 5,000). The authenticator execution costs are the chain-policy values for the
/// enshrined canonical authenticators, pinned to the EVM precompile costs Base
/// uses (see the crate docs).
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
    /// `nonce_key_cost` for nonce-free (`NONCE_KEY_MAX`) transactions: the
    /// expiring-nonce replay-protection state (2 cold SLOADs + 1 warm SLOAD + 3
    /// warm SSTORE resets).
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

    // ── Enshrined authenticator execution gas (chain policy) ─────────────────
    /// secp256k1 (`ECRECOVER` sentinel / EOA path) execution gas — the
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
        if authenticator == Eip8130Constants::ECRECOVER_AUTHENTICATOR {
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
