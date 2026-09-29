//! Storage layouts of supported Chainlink aggregator implementations.

use alloy_primitives::{U256, keccak256};

/// Storage layout of a Chainlink aggregator implementation.
///
/// Every supported layout packs `latestAggregatorRoundId` into the `s_hotVars`
/// word at [`Self::ROUND_ID_BIT_OFFSET`] and stores each round's `Transmission`
/// in one slot of the `s_transmissions` mapping. Only the slot indices differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChainlinkLayout {
    /// `AccessControlledOCR2Aggregator 1.0.0`, the standard data feed aggregator.
    Ocr2,
    /// `DualAggregator 1.0.0`, the aggregator behind Chainlink SVR feeds.
    Dual,
}

impl ChainlinkLayout {
    /// `typeAndVersion()` reported by [`Self::Ocr2`] aggregators.
    pub const OCR2_TYPE_AND_VERSION: &'static str = "AccessControlledOCR2Aggregator 1.0.0";

    /// `typeAndVersion()` reported by [`Self::Dual`] aggregators.
    pub const DUAL_TYPE_AND_VERSION: &'static str = "DualAggregator 1.0.0";

    /// Bit offset of `uint32 latestAggregatorRoundId` in the `s_hotVars` word,
    /// after `uint8 f` and `uint40 latestEpochAndRound`.
    pub const ROUND_ID_BIT_OFFSET: usize = 48;

    /// Returns the layout of the aggregator reporting `type_and_version`, or
    /// `None` for an implementation whose layout is unknown.
    pub fn from_type_and_version(type_and_version: &str) -> Option<Self> {
        match type_and_version {
            Self::OCR2_TYPE_AND_VERSION => Some(Self::Ocr2),
            Self::DUAL_TYPE_AND_VERSION => Some(Self::Dual),
            _ => None,
        }
    }

    /// Slot of the `s_hotVars` word.
    pub const fn hot_vars_slot(self) -> U256 {
        match self {
            Self::Ocr2 => U256::from_limbs([11, 0, 0, 0]),
            Self::Dual => U256::from_limbs([13, 0, 0, 0]),
        }
    }

    /// Base slot of the `s_transmissions` mapping.
    pub const fn transmissions_slot(self) -> U256 {
        match self {
            Self::Ocr2 => U256::from_limbs([12, 0, 0, 0]),
            Self::Dual => U256::from_limbs([17, 0, 0, 0]),
        }
    }

    /// Slot of `s_transmissions[round_id]`.
    pub fn transmission_slot(self, round_id: u32) -> U256 {
        let mut preimage = [0u8; 64];
        preimage[..32].copy_from_slice(&U256::from(round_id).to_be_bytes::<32>());
        preimage[32..].copy_from_slice(&self.transmissions_slot().to_be_bytes::<32>());
        U256::from_be_bytes(keccak256(preimage).0)
    }

    /// Extracts `latestAggregatorRoundId` from an `s_hotVars` word.
    pub fn latest_round_id(hot_vars: U256) -> u32 {
        (hot_vars >> Self::ROUND_ID_BIT_OFFSET).as_limbs()[0] as u32
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::uint;

    use super::*;

    #[test]
    fn selects_layout_from_type_and_version() {
        assert_eq!(
            ChainlinkLayout::from_type_and_version("AccessControlledOCR2Aggregator 1.0.0"),
            Some(ChainlinkLayout::Ocr2)
        );
        assert_eq!(
            ChainlinkLayout::from_type_and_version("DualAggregator 1.0.0"),
            Some(ChainlinkLayout::Dual)
        );
        assert_eq!(ChainlinkLayout::from_type_and_version("OCR2Aggregator 2.0.0"), None);
    }

    /// `s_hotVars` words and `cast index uint256 <round> <slot>` keys read from
    /// the Base mainnet USDC/USD (OCR2) and ETH/USD (Dual) aggregators at block
    /// 51951686.
    #[test]
    fn locates_mainnet_rounds() {
        let ocr2_hot_vars = uint!(0x2900004b9a0603_U256);
        assert_eq!(ChainlinkLayout::latest_round_id(ocr2_hot_vars), 41);
        assert_eq!(
            ChainlinkLayout::Ocr2.transmission_slot(41),
            uint!(0x9a86beb6228bf168307f184cf63c593599ffdab1a9a2399ca6e1c21e6fff27ee_U256)
        );

        let dual_hot_vars = uint!(0x17500000176200002e720403_U256);
        assert_eq!(ChainlinkLayout::latest_round_id(dual_hot_vars), 5986);
        assert_eq!(
            ChainlinkLayout::Dual.transmission_slot(5986),
            uint!(0xb792de9466fa9106f2561552aef1cd6ad2afc977caf768b05b362ef803d1fde4_U256)
        );
    }

    #[test]
    fn round_id_ignores_neighbouring_fields() {
        let hot_vars = (U256::MAX << 80) | (U256::from(7u32) << 48) | U256::from(u64::MAX >> 16);

        assert_eq!(ChainlinkLayout::latest_round_id(hot_vars), 7);
    }
}
