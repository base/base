//! EVM configuration feature bitmap.

/// EVM configuration feature bitmap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EvmFeatures(u64);

impl EvmFeatures {
    /// Creates an empty feature bitmap.
    #[inline]
    pub const fn empty() -> Self {
        Self(0)
    }

    const fn from_bit(bit: u32) -> Self {
        Self(1 << bit)
    }

    /// Returns `true` if no feature bits are set.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns `true` if all `other` feature bits are set.
    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns `true` if any `other` feature bits are set.
    #[inline]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Inserts feature bits.
    #[inline]
    pub const fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Removes feature bits.
    #[inline]
    pub const fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    /// Sets or clears feature bits.
    #[inline]
    pub const fn set(&mut self, other: Self, on: bool) {
        if on {
            self.insert(other);
        } else {
            self.remove(other);
        }
    }
}

impl core::ops::BitOr for EvmFeatures {
    type Output = Self;

    #[inline]
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for EvmFeatures {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        self.insert(rhs);
    }
}

impl core::ops::BitAnd for EvmFeatures {
    type Output = Self;

    #[inline]
    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl core::ops::BitAndAssign for EvmFeatures {
    #[inline]
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl core::ops::Not for EvmFeatures {
    type Output = Self;

    #[inline]
    fn not(self) -> Self::Output {
        Self(!self.0)
    }
}

macro_rules! evm_features {
    (@const $bit:expr;) => {};
    (@const $bit:expr; $(#[$attr:meta])* $name:ident, $($rest:tt)*) => {
        $(#[$attr])*
        pub const $name: Self = Self::from_bit($bit);
        evm_features!(@const $bit + 1; $($rest)*);
    };
    ($($tokens:tt)*) => {
        impl EvmFeatures {
            evm_features!(@const 0; $($tokens)*);
        }
    };
}

evm_features! {
    /// Checks transaction chain IDs.
    ///
    /// Default: on
    TX_CHAIN_ID_CHECK,
    /// Checks transaction nonces against account nonces.
    ///
    /// Default: on
    NONCE_CHECK,
    /// Checks that senders can pay transaction costs.
    ///
    /// Default: on
    BALANCE_CHECK,
    /// Tops up the sender's native balance when [`Self::BALANCE_CHECK`] is disabled.
    ///
    /// This preserves Ethereum simulation behavior while allowing custom fee systems to disable
    /// native-balance validation without minting a synthetic native balance.
    ///
    /// Default: on
    BALANCE_TOP_UP,
    /// Checks that transaction gas limits do not exceed the block gas limit.
    ///
    /// Default: on
    BLOCK_GAS_LIMIT_CHECK,
    /// Applies [EIP-3607](https://eips.ethereum.org/EIPS/eip-3607) sender code rejection.
    ///
    /// Default: on
    EIP3607,
    /// Checks [EIP-1559](https://eips.ethereum.org/EIPS/eip-1559) max priority fee against max fee.
    ///
    /// Default: on
    PRIORITY_FEE_CHECK,
    /// Charges transaction fees.
    ///
    /// Default: on
    FEE_CHARGE,
    /// Applies [EIP-2](https://eips.ethereum.org/EIPS/eip-2) create transaction intrinsic gas.
    ///
    /// Default: on since Homestead
    EIP2,
    /// Applies [EIP-150](https://eips.ethereum.org/EIPS/eip-150) call gas forwarding limits.
    ///
    /// Default: on since Tangerine Whistle
    EIP150,
    /// Applies [EIP-161](https://eips.ethereum.org/EIPS/eip-161) state clearing rules.
    ///
    /// Default: on since Spurious Dragon
    EIP161,
    /// Checks deployed contract bytecode sizes against the active size limit.
    ///
    /// Default: on since Spurious Dragon
    CODE_SIZE_CHECK,
    /// Applies [EIP-2028](https://eips.ethereum.org/EIPS/eip-2028) transaction calldata repricing.
    ///
    /// Default: on since Istanbul
    EIP2028,
    /// Applies [EIP-2200](https://eips.ethereum.org/EIPS/eip-2200) SSTORE net metering.
    ///
    /// Default: on since Istanbul
    EIP2200,
    /// Applies [EIP-2929](https://eips.ethereum.org/EIPS/eip-2929) warm/cold access rules.
    ///
    /// Default: on since Berlin
    EIP2929,
    /// Applies [EIP-3529](https://eips.ethereum.org/EIPS/eip-3529) refund reductions.
    ///
    /// Default: on since London
    EIP3529,
    /// Applies [EIP-3541](https://eips.ethereum.org/EIPS/eip-3541) contract code prefix rejection.
    ///
    /// Default: on since London
    EIP3541,
    /// Checks [EIP-1559](https://eips.ethereum.org/EIPS/eip-1559) transaction fee caps against the block base fee.
    ///
    /// Default: on since London
    BASE_FEE_CHECK,
    /// Applies [EIP-4399](https://eips.ethereum.org/EIPS/eip-4399) PREVRANDAO opcode semantics.
    ///
    /// Default: on since Merge
    EIP4399,
    /// Applies [EIP-3651](https://eips.ethereum.org/EIPS/eip-3651) warm coinbase at transaction start.
    ///
    /// Default: on since Shanghai
    EIP3651,
    /// Applies [EIP-3860](https://eips.ethereum.org/EIPS/eip-3860) initcode size limits and word gas.
    ///
    /// Default: on since Shanghai
    EIP3860,
    /// Applies [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) SELFDESTRUCT restrictions.
    ///
    /// Default: on since Cancun
    EIP6780,
    /// Applies [EIP-7623](https://eips.ethereum.org/EIPS/eip-7623) calldata cost floor.
    ///
    /// Default: on since Prague
    EIP7623,
    /// Applies [EIP-7702](https://eips.ethereum.org/EIPS/eip-7702) delegation designators.
    ///
    /// Default: on since Prague
    EIP7702,
    /// Applies [EIP-8037](https://eips.ethereum.org/EIPS/eip-8037) state creation gas accounting.
    ///
    /// Default: on since Amsterdam
    EIP8037,
    /// Applies [EIP-7708](https://eips.ethereum.org/EIPS/eip-7708) ETH transfer logs.
    ///
    /// Default: on since Amsterdam
    EIP7708,
    /// Applies [EIP-8246](https://eips.ethereum.org/EIPS/eip-8246) SELFDESTRUCT balance-burn removal.
    ///
    /// Self-destructed accounts keep their balance instead of burning it; at finalization they are
    /// reset to balance-only accounts (nonce 0, no code, no storage) rather than deleted.
    ///
    /// Default: on since Amsterdam
    EIP8246,
    /// Applies [EIP-2780](https://eips.ethereum.org/EIPS/eip-2780) reduced intrinsic transaction
    /// gas: a decomposed `TX_BASE_COST + to-based + value-based` intrinsic model plus top-level
    /// execution charges for empty recipients with value and EIP-7702-delegated recipients.
    ///
    /// Default: on since Amsterdam
    EIP2780,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_bitmap_api() {
        let mut features = EvmFeatures::TX_CHAIN_ID_CHECK | EvmFeatures::NONCE_CHECK;
        assert!(features.contains(EvmFeatures::TX_CHAIN_ID_CHECK));
        assert!(features.intersects(EvmFeatures::NONCE_CHECK));

        features.remove(EvmFeatures::NONCE_CHECK);
        assert!(!features.contains(EvmFeatures::NONCE_CHECK));

        features.set(EvmFeatures::BASE_FEE_CHECK, true);
        assert_eq!(features, EvmFeatures::TX_CHAIN_ID_CHECK | EvmFeatures::BASE_FEE_CHECK);
    }
}
