use alloy_chains::{Chain, NamedChain};
use alloy_hardforks::{ForkCondition, hardfork};

use crate::{
    BASE_MAINNET_BEDROCK_BLOCK, BASE_MAINNET_CANYON_TIMESTAMP, BASE_MAINNET_ECOTONE_TIMESTAMP,
    BASE_MAINNET_FJORD_TIMESTAMP, BASE_MAINNET_GRANITE_TIMESTAMP, BASE_MAINNET_HOLOCENE_TIMESTAMP,
    BASE_MAINNET_ISTHMUS_TIMESTAMP, BASE_MAINNET_JOVIAN_TIMESTAMP, BASE_MAINNET_REGOLITH_TIMESTAMP,
    BASE_SEPOLIA_BEDROCK_BLOCK, BASE_SEPOLIA_CANYON_TIMESTAMP, BASE_SEPOLIA_ECOTONE_TIMESTAMP,
    BASE_SEPOLIA_FJORD_TIMESTAMP, BASE_SEPOLIA_GRANITE_TIMESTAMP, BASE_SEPOLIA_HOLOCENE_TIMESTAMP,
    BASE_SEPOLIA_ISTHMUS_TIMESTAMP, BASE_SEPOLIA_JOVIAN_TIMESTAMP, BASE_SEPOLIA_REGOLITH_TIMESTAMP,
};

hardfork!(
    /// The name of an optimism hardfork.
    ///
    /// When building a list of hardforks for a chain, it's still expected to zip with
    /// [`EthereumHardfork`](alloy_hardforks::EthereumHardfork).
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[derive(Default)]
    OpHardfork {
        /// Bedrock: <https://blog.oplabs.co/introducing-optimism-bedrock>.
        Bedrock,
        /// Regolith: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#regolith>.
        Regolith,
        /// <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#canyon>.
        Canyon,
        /// Ecotone: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#ecotone>.
        Ecotone,
        /// Fjord: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#fjord>
        Fjord,
        /// Granite: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#granite>
        Granite,
        /// Holocene: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#holocene>
        Holocene,
        /// Isthmus: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/isthmus/overview.md>
        #[default]
        Isthmus,
        /// Jovian: <https://github.com/ethereum-optimism/specs/tree/main/specs/protocol/jovian>
        Jovian,
    }
);

impl OpHardfork {
    /// Reverse lookup to find the hardfork given a chain ID and block timestamp.
    /// Returns the active hardfork at the given timestamp for the specified OP chain.
    pub fn from_chain_and_timestamp(chain: Chain, timestamp: u64) -> Option<Self> {
        let named = chain.named()?;

        match named {
            NamedChain::Base => Some(match timestamp {
                _i if timestamp < BASE_MAINNET_CANYON_TIMESTAMP => Self::Regolith,
                _i if timestamp < BASE_MAINNET_ECOTONE_TIMESTAMP => Self::Canyon,
                _i if timestamp < BASE_MAINNET_FJORD_TIMESTAMP => Self::Ecotone,
                _i if timestamp < BASE_MAINNET_GRANITE_TIMESTAMP => Self::Fjord,
                _i if timestamp < BASE_MAINNET_HOLOCENE_TIMESTAMP => Self::Granite,
                _i if timestamp < BASE_MAINNET_ISTHMUS_TIMESTAMP => Self::Holocene,
                _i if timestamp < BASE_MAINNET_JOVIAN_TIMESTAMP => Self::Isthmus,
                _ => Self::Jovian,
            }),
            NamedChain::BaseSepolia => Some(match timestamp {
                _i if timestamp < BASE_SEPOLIA_CANYON_TIMESTAMP => Self::Regolith,
                _i if timestamp < BASE_SEPOLIA_ECOTONE_TIMESTAMP => Self::Canyon,
                _i if timestamp < BASE_SEPOLIA_FJORD_TIMESTAMP => Self::Ecotone,
                _i if timestamp < BASE_SEPOLIA_GRANITE_TIMESTAMP => Self::Fjord,
                _i if timestamp < BASE_SEPOLIA_HOLOCENE_TIMESTAMP => Self::Granite,
                _i if timestamp < BASE_SEPOLIA_ISTHMUS_TIMESTAMP => Self::Holocene,
                _i if timestamp < BASE_SEPOLIA_JOVIAN_TIMESTAMP => Self::Isthmus,
                _ => Self::Jovian,
            }),
            _ => None,
        }
    }

    /// Base mainnet list of hardforks.
    pub const fn base_mainnet() -> [(Self, ForkCondition); 9] {
        [
            (Self::Bedrock, ForkCondition::Block(BASE_MAINNET_BEDROCK_BLOCK)),
            (Self::Regolith, ForkCondition::Timestamp(BASE_MAINNET_REGOLITH_TIMESTAMP)),
            (Self::Canyon, ForkCondition::Timestamp(BASE_MAINNET_CANYON_TIMESTAMP)),
            (Self::Ecotone, ForkCondition::Timestamp(BASE_MAINNET_ECOTONE_TIMESTAMP)),
            (Self::Fjord, ForkCondition::Timestamp(BASE_MAINNET_FJORD_TIMESTAMP)),
            (Self::Granite, ForkCondition::Timestamp(BASE_MAINNET_GRANITE_TIMESTAMP)),
            (Self::Holocene, ForkCondition::Timestamp(BASE_MAINNET_HOLOCENE_TIMESTAMP)),
            (Self::Isthmus, ForkCondition::Timestamp(BASE_MAINNET_ISTHMUS_TIMESTAMP)),
            (Self::Jovian, ForkCondition::Timestamp(BASE_MAINNET_JOVIAN_TIMESTAMP)),
        ]
    }

    /// Base Sepolia list of hardforks.
    pub const fn base_sepolia() -> [(Self, ForkCondition); 9] {
        [
            (Self::Bedrock, ForkCondition::Block(BASE_SEPOLIA_BEDROCK_BLOCK)),
            (Self::Regolith, ForkCondition::Timestamp(BASE_SEPOLIA_REGOLITH_TIMESTAMP)),
            (Self::Canyon, ForkCondition::Timestamp(BASE_SEPOLIA_CANYON_TIMESTAMP)),
            (Self::Ecotone, ForkCondition::Timestamp(BASE_SEPOLIA_ECOTONE_TIMESTAMP)),
            (Self::Fjord, ForkCondition::Timestamp(BASE_SEPOLIA_FJORD_TIMESTAMP)),
            (Self::Granite, ForkCondition::Timestamp(BASE_SEPOLIA_GRANITE_TIMESTAMP)),
            (Self::Holocene, ForkCondition::Timestamp(BASE_SEPOLIA_HOLOCENE_TIMESTAMP)),
            (Self::Isthmus, ForkCondition::Timestamp(BASE_SEPOLIA_ISTHMUS_TIMESTAMP)),
            (Self::Jovian, ForkCondition::Timestamp(BASE_SEPOLIA_JOVIAN_TIMESTAMP)),
        ]
    }

    /// Devnet list of hardforks.
    pub const fn devnet() -> [(Self, ForkCondition); 9] {
        [
            (Self::Bedrock, ForkCondition::ZERO_BLOCK),
            (Self::Regolith, ForkCondition::ZERO_TIMESTAMP),
            (Self::Canyon, ForkCondition::ZERO_TIMESTAMP),
            (Self::Ecotone, ForkCondition::ZERO_TIMESTAMP),
            (Self::Fjord, ForkCondition::ZERO_TIMESTAMP),
            (Self::Granite, ForkCondition::ZERO_TIMESTAMP),
            (Self::Holocene, ForkCondition::ZERO_TIMESTAMP),
            (Self::Isthmus, ForkCondition::ZERO_TIMESTAMP),
            (Self::Jovian, ForkCondition::ZERO_TIMESTAMP),
        ]
    }

    /// Returns index of `self` in sorted canonical array.
    pub const fn idx(&self) -> usize {
        *self as usize
    }
}

#[cfg(test)]
mod tests {
    use core::str::FromStr;

    use super::*;

    extern crate alloc;

    #[test]
    fn check_op_hardfork_from_str() {
        let hardfork_str = [
            "beDrOck", "rEgOlITH", "cAnYoN", "eCoToNe", "FJorD", "GRaNiTe", "hOlOcEnE", "isthMUS",
            "jOvIaN",
        ];
        let expected_hardforks = [
            OpHardfork::Bedrock,
            OpHardfork::Regolith,
            OpHardfork::Canyon,
            OpHardfork::Ecotone,
            OpHardfork::Fjord,
            OpHardfork::Granite,
            OpHardfork::Holocene,
            OpHardfork::Isthmus,
            OpHardfork::Jovian,
        ];

        let hardforks: alloc::vec::Vec<OpHardfork> =
            hardfork_str.iter().map(|h| OpHardfork::from_str(h).unwrap()).collect();

        assert_eq!(hardforks, expected_hardforks);
    }

    #[test]
    fn check_nonexistent_hardfork_from_str() {
        assert!(OpHardfork::from_str("not a hardfork").is_err());
    }

    #[test]
    fn test_reverse_lookup_op_chains() {
        let test_cases = [
            (Chain::base_mainnet(), BASE_MAINNET_CANYON_TIMESTAMP, OpHardfork::Canyon),
            (Chain::base_mainnet(), BASE_MAINNET_ECOTONE_TIMESTAMP, OpHardfork::Ecotone),
            (Chain::base_mainnet(), BASE_MAINNET_JOVIAN_TIMESTAMP, OpHardfork::Jovian),
            (Chain::base_sepolia(), BASE_SEPOLIA_CANYON_TIMESTAMP, OpHardfork::Canyon),
            (Chain::base_sepolia(), BASE_SEPOLIA_ECOTONE_TIMESTAMP, OpHardfork::Ecotone),
            (Chain::base_sepolia(), BASE_SEPOLIA_JOVIAN_TIMESTAMP, OpHardfork::Jovian),
        ];

        for (chain_id, timestamp, expected) in test_cases {
            assert_eq!(
                OpHardfork::from_chain_and_timestamp(chain_id, timestamp),
                Some(expected),
                "chain {chain_id} at timestamp {timestamp}"
            );
        }

        assert_eq!(OpHardfork::from_chain_and_timestamp(Chain::from_id(999999), 1000000), None);
    }
}
