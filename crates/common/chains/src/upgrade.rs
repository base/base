use alloy_hardforks::{ForkCondition, hardfork};

use crate::ChainConfig;

hardfork!(
    /// The name of a Base network upgrade.
    ///
    /// When building a list of upgrades for a chain, it's still expected to zip with
    /// [`EthereumHardfork`](alloy_hardforks::EthereumHardfork).
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[derive(Default)]
    BaseUpgrade {
        /// Bedrock: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#bedrock>.
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
        /// Azul: First Base-specific network upgrade.
        Azul,
    }
);

impl BaseUpgrade {
    /// Returns the list of upgrades with their activation conditions for the given chain config.
    pub const fn forks_for(cfg: &ChainConfig) -> [(Self, ForkCondition); 10] {
        let azul = match cfg.azul_timestamp {
            Some(ts) => ForkCondition::Timestamp(ts),
            None => ForkCondition::Never,
        };
        [
            (Self::Bedrock, ForkCondition::Block(cfg.bedrock_block)),
            (Self::Regolith, ForkCondition::Timestamp(cfg.regolith_timestamp)),
            (Self::Canyon, ForkCondition::Timestamp(cfg.canyon_timestamp)),
            (Self::Ecotone, ForkCondition::Timestamp(cfg.ecotone_timestamp)),
            (Self::Fjord, ForkCondition::Timestamp(cfg.fjord_timestamp)),
            (Self::Granite, ForkCondition::Timestamp(cfg.granite_timestamp)),
            (Self::Holocene, ForkCondition::Timestamp(cfg.holocene_timestamp)),
            (Self::Isthmus, ForkCondition::Timestamp(cfg.isthmus_timestamp)),
            (Self::Jovian, ForkCondition::Timestamp(cfg.jovian_timestamp)),
            (Self::Azul, azul),
        ]
    }

    /// Base mainnet list of upgrades.
    pub const fn mainnet() -> [(Self, ForkCondition); 10] {
        Self::forks_for(ChainConfig::mainnet())
    }

    /// Base Sepolia list of upgrades.
    pub const fn sepolia() -> [(Self, ForkCondition); 10] {
        Self::forks_for(ChainConfig::sepolia())
    }

    /// Devnet list of upgrades.
    pub const fn devnet() -> [(Self, ForkCondition); 10] {
        Self::forks_for(ChainConfig::devnet())
    }

    /// Base devnet-0-sepolia-dev-0 list of upgrades.
    pub const fn base_devnet_0_sepolia_dev_0() -> [(Self, ForkCondition); 10] {
        Self::forks_for(ChainConfig::alpha())
    }

    /// Base Zeronet list of upgrades.
    pub const fn zeronet() -> [(Self, ForkCondition); 10] {
        Self::forks_for(ChainConfig::zeronet())
    }

    /// Returns index of `self` in sorted canonical array.
    pub const fn idx(&self) -> usize {
        *self as usize
    }
}

#[cfg(test)]
mod tests {
    use core::str::FromStr;

    use alloy_chains::Chain;

    use super::*;

    extern crate alloc;

    #[test]
    fn check_base_upgrade_from_str() {
        let upgrade_str = [
            "beDrOck", "rEgOlITH", "cAnYoN", "eCoToNe", "FJorD", "GRaNiTe", "hOlOcEnE", "isthMUS",
            "jOvIaN", "aZuL",
        ];
        let expected_upgrades = [
            BaseUpgrade::Bedrock,
            BaseUpgrade::Regolith,
            BaseUpgrade::Canyon,
            BaseUpgrade::Ecotone,
            BaseUpgrade::Fjord,
            BaseUpgrade::Granite,
            BaseUpgrade::Holocene,
            BaseUpgrade::Isthmus,
            BaseUpgrade::Jovian,
            BaseUpgrade::Azul,
        ];

        let upgrades: alloc::vec::Vec<BaseUpgrade> =
            upgrade_str.iter().map(|h| BaseUpgrade::from_str(h).unwrap()).collect();

        assert_eq!(upgrades, expected_upgrades);
    }

    #[test]
    fn check_nonexistent_upgrade_from_str() {
        assert!(BaseUpgrade::from_str("not an upgrade").is_err());
    }

    /// Reverse lookup to find the upgrade given a chain ID and block timestamp.
    /// Returns the active upgrade at the given timestamp for the specified Base chain.
    fn upgrade_from_chain_and_timestamp(chain: Chain, timestamp: u64) -> Option<BaseUpgrade> {
        let cfg = ChainConfig::by_chain_id(chain.id())?;
        Some(match timestamp {
            _ if timestamp < cfg.canyon_timestamp => BaseUpgrade::Regolith,
            _ if timestamp < cfg.ecotone_timestamp => BaseUpgrade::Canyon,
            _ if timestamp < cfg.fjord_timestamp => BaseUpgrade::Ecotone,
            _ if timestamp < cfg.granite_timestamp => BaseUpgrade::Fjord,
            _ if timestamp < cfg.holocene_timestamp => BaseUpgrade::Granite,
            _ if timestamp < cfg.isthmus_timestamp => BaseUpgrade::Holocene,
            _ if timestamp < cfg.jovian_timestamp => BaseUpgrade::Isthmus,
            _ if cfg.azul_timestamp.is_some_and(|azul| timestamp >= azul) => BaseUpgrade::Azul,
            _ => BaseUpgrade::Jovian,
        })
    }

    #[test]
    fn test_reverse_lookup_base_chains() {
        let test_cases = [
            (Chain::base_mainnet(), ChainConfig::mainnet().canyon_timestamp, BaseUpgrade::Canyon),
            (Chain::base_mainnet(), ChainConfig::mainnet().ecotone_timestamp, BaseUpgrade::Ecotone),
            (Chain::base_mainnet(), ChainConfig::mainnet().jovian_timestamp, BaseUpgrade::Jovian),
            (Chain::base_sepolia(), ChainConfig::sepolia().canyon_timestamp, BaseUpgrade::Canyon),
            (Chain::base_sepolia(), ChainConfig::sepolia().ecotone_timestamp, BaseUpgrade::Ecotone),
            (Chain::base_sepolia(), ChainConfig::sepolia().jovian_timestamp, BaseUpgrade::Jovian),
        ];

        for (chain_id, timestamp, expected) in test_cases {
            assert_eq!(
                upgrade_from_chain_and_timestamp(chain_id, timestamp),
                Some(expected),
                "chain {chain_id} at timestamp {timestamp}"
            );
        }

        assert_eq!(upgrade_from_chain_and_timestamp(Chain::from_id(999999), 1000000), None);
    }
}
