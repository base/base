//! Gas schedule native precompiles charge for their storage, log, code, and hashing work.
//!
//! base-reth builds the precompile storage provider with revm's `GasParams::default()`, which
//! is the Osaka table regardless of the active fork. These constants are that table's values,
//! and the SSTORE functions are revm's EIP-2200/2929/3529 rules over them, so a native precompile
//! charges exactly what it charges in base-reth.

use alloy_primitives::U256;

/// Original, present, and new values of a slot written by SSTORE, for net gas metering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SstoreValues {
    /// Value at the start of the transaction.
    pub original: U256,
    /// Value before this write.
    pub present: U256,
    /// Value written.
    pub new: U256,
}

/// The fixed (Osaka) gas table native precompiles charge.
#[derive(Clone, Copy, Debug)]
pub struct GasSchedule;

impl GasSchedule {
    /// EIP-2929 warm storage read (also the warm account access cost).
    pub const WARM_STORAGE_READ: u64 = 100;
    /// EIP-2929 extra cost of a cold account access.
    pub const COLD_ACCOUNT_ADDITIONAL: u64 = 2_500;
    /// EIP-2929 extra cost of a cold storage read on SLOAD.
    pub const COLD_STORAGE_ADDITIONAL: u64 = 2_000;
    /// EIP-2929 cold storage cost charged by SSTORE.
    pub const COLD_STORAGE: u64 = 2_100;
    /// Static part of SSTORE.
    pub const SSTORE_STATIC: u64 = 100;
    /// SSTORE creating a slot, beyond the static part.
    pub const SSTORE_SET: u64 = 19_900;
    /// SSTORE changing a clean non-zero slot, beyond the static part.
    pub const SSTORE_RESET: u64 = 2_800;
    /// EIP-3529 refund for clearing a slot.
    pub const SSTORE_CLEARING_REFUND: i64 = 4_800;
    /// EIP-2200 call stipend: SSTORE halts at or below this remaining gas.
    pub const CALL_STIPEND: u64 = 2_300;
    /// Base LOG cost.
    pub const LOG: u64 = 375;
    /// LOG cost per topic.
    pub const LOG_TOPIC: u64 = 375;
    /// LOG cost per data byte.
    pub const LOG_DATA: u64 = 8;
    /// Code deposit cost per byte.
    pub const CODE_DEPOSIT: u64 = 200;
    /// CREATE base cost.
    pub const CREATE: u64 = 32_000;
    /// KECCAK256 base cost.
    pub const KECCAK256: u64 = 30;
    /// KECCAK256 cost per 32-byte word.
    pub const KECCAK256_WORD: u64 = 6;

    /// Dynamic SSTORE cost after [`Self::SSTORE_STATIC`]: the cold surcharge plus the
    /// set or reset cost when the write changes a clean slot.
    pub const fn sstore_dynamic(values: SstoreValues, is_cold: bool) -> u64 {
        let mut gas = 0;
        if is_cold {
            gas += Self::COLD_STORAGE;
        }
        let changes_present = !values.new.const_eq(&values.present);
        let is_clean = values.original.const_eq(&values.present);
        if changes_present && is_clean {
            gas +=
                if values.original.const_is_zero() { Self::SSTORE_SET } else { Self::SSTORE_RESET };
        }
        gas
    }

    /// EIP-2200 / EIP-3529 SSTORE refund (may be negative).
    pub const fn sstore_refund(values: SstoreValues) -> i64 {
        if values.new.const_eq(&values.present) {
            return 0;
        }
        if values.original.const_eq(&values.present) && values.new.const_is_zero() {
            return Self::SSTORE_CLEARING_REFUND;
        }
        let mut refund = 0;
        if !values.original.const_is_zero() {
            if values.present.const_is_zero() {
                refund -= Self::SSTORE_CLEARING_REFUND;
            } else if values.new.const_is_zero() {
                refund += Self::SSTORE_CLEARING_REFUND;
            }
        }
        if values.original.const_eq(&values.new) {
            refund += if values.original.const_is_zero() {
                Self::SSTORE_SET as i64
            } else {
                Self::SSTORE_RESET as i64
            };
        }
        refund
    }

    /// LOG cost for `topics` topics and `data_len` data bytes, including the base cost.
    pub const fn log(topics: u64, data_len: u64) -> u64 {
        Self::LOG
            .saturating_add(Self::LOG_DATA.saturating_mul(data_len))
            .saturating_add(Self::LOG_TOPIC * topics)
    }

    /// KECCAK256 cost over `len` bytes (base plus per-word).
    pub const fn keccak256(len: usize) -> u64 {
        let words = len.div_ceil(32) as u64;
        Self::KECCAK256.saturating_add(Self::KECCAK256_WORD.saturating_mul(words))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(original: u64, present: u64, new: u64) -> SstoreValues {
        SstoreValues {
            original: U256::from(original),
            present: U256::from(present),
            new: U256::from(new),
        }
    }

    #[test]
    fn sstore_costs_follow_eip2200_net_metering() {
        // Fresh slot creation, cold: 2100 + 19900 dynamic.
        assert_eq!(GasSchedule::sstore_dynamic(values(0, 0, 1), true), 22_000);
        // Dirty rewrite, warm: no dynamic cost.
        assert_eq!(GasSchedule::sstore_dynamic(values(0, 1, 2), false), 0);
        // Clean non-zero reset, warm: 2800.
        assert_eq!(GasSchedule::sstore_dynamic(values(5, 5, 6), false), 2_800);
        // Clearing a clean slot refunds 4800; restoring 0→1→0 refunds the set cost.
        assert_eq!(GasSchedule::sstore_refund(values(5, 5, 0)), 4_800);
        assert_eq!(GasSchedule::sstore_refund(values(0, 1, 0)), 19_900);
        // Un-clearing a slot cleared earlier in the transaction takes the refund back.
        assert_eq!(GasSchedule::sstore_refund(values(5, 0, 6)), -4_800);
        assert_eq!(GasSchedule::sstore_refund(values(5, 0, 5)), -4_800 + 2_800);
    }
}
