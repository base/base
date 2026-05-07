use alloy_hardforks::{ForkCondition, hardfork};
use revm::primitives::hardfork::SpecId;

use crate::{ChainConfig, Upgrades};

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
        Isthmus,
        /// Jovian: Base network upgrade.
        Jovian,
        /// Azul: First Base-specific network upgrade.
        #[default]
        Azul,
        /// Beryl: Second Base-specific network upgrade.
        Beryl,
    }
);

impl BaseUpgrade {
    /// Latest Base upgrade used by default.
    pub const LATEST: Self = Self::Azul;

    /// Converts the Base upgrade into its matching Ethereum execution spec.
    pub const fn into_eth_spec(self) -> SpecId {
        match self {
            Self::Bedrock | Self::Regolith => SpecId::MERGE,
            Self::Canyon => SpecId::SHANGHAI,
            Self::Ecotone | Self::Fjord | Self::Granite | Self::Holocene => SpecId::CANCUN,
            Self::Isthmus | Self::Jovian => SpecId::PRAGUE,
            // Azul, Beryl, and newer Base upgrades inherit the latest known Ethereum spec until
            // explicitly mapped.
            _ => SpecId::OSAKA,
        }
    }

    /// Returns the active Base upgrade at the given timestamp.
    ///
    /// This is intended for post-Bedrock timestamp-based fork resolution.
    pub fn from_timestamp(chain_spec: impl Upgrades, timestamp: u64) -> Self {
        if chain_spec.is_beryl_active_at_timestamp(timestamp) {
            Self::Beryl
        } else if chain_spec.is_azul_active_at_timestamp(timestamp) {
            Self::Azul
        } else if chain_spec.is_jovian_active_at_timestamp(timestamp) {
            Self::Jovian
        } else if chain_spec.is_isthmus_active_at_timestamp(timestamp) {
            Self::Isthmus
        } else if chain_spec.is_holocene_active_at_timestamp(timestamp) {
            Self::Holocene
        } else if chain_spec.is_granite_active_at_timestamp(timestamp) {
            Self::Granite
        } else if chain_spec.is_fjord_active_at_timestamp(timestamp) {
            Self::Fjord
        } else if chain_spec.is_ecotone_active_at_timestamp(timestamp) {
            Self::Ecotone
        } else if chain_spec.is_canyon_active_at_timestamp(timestamp) {
            Self::Canyon
        } else if chain_spec.is_regolith_active_at_timestamp(timestamp) {
            Self::Regolith
        } else {
            Self::Bedrock
        }
    }

    /// Returns the list of upgrades with their activation conditions for the given chain config.
    pub const fn forks_for(cfg: &ChainConfig) -> [(Self, ForkCondition); 11] {
        let azul = match cfg.azul_timestamp {
            Some(ts) => ForkCondition::Timestamp(ts),
            None => ForkCondition::Never,
        };
        let beryl = match cfg.beryl_timestamp {
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
            (Self::Beryl, beryl),
        ]
    }

    /// Base mainnet list of upgrades.
    pub const fn mainnet() -> [(Self, ForkCondition); 11] {
        Self::forks_for(ChainConfig::mainnet())
    }

    /// Base Sepolia list of upgrades.
    pub const fn sepolia() -> [(Self, ForkCondition); 11] {
        Self::forks_for(ChainConfig::sepolia())
    }

    /// Devnet list of upgrades.
    pub const fn devnet() -> [(Self, ForkCondition); 11] {
        Self::forks_for(ChainConfig::devnet())
    }

    /// Base Zeronet list of upgrades.
    pub const fn zeronet() -> [(Self, ForkCondition); 11] {
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
            "jOvIaN", "aZuL", "bErYl",
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
            BaseUpgrade::Beryl,
        ];

        let upgrades: alloc::vec::Vec<BaseUpgrade> =
            upgrade_str.iter().map(|h| BaseUpgrade::from_str(h).unwrap()).collect();

        assert_eq!(upgrades, expected_upgrades);
    }

    #[test]
    fn check_nonexistent_upgrade_from_str() {
        assert!(BaseUpgrade::from_str("not an upgrade").is_err());
    }

    #[test]
    fn latest_base_upgrade_matches_default() {
        assert_eq!(BaseUpgrade::default(), BaseUpgrade::LATEST);
        assert_eq!(BaseUpgrade::LATEST, BaseUpgrade::Azul);
    }

    #[test]
    fn check_base_upgrade_eth_spec_mapping() {
        let test_cases = [
            (BaseUpgrade::Bedrock, SpecId::MERGE),
            (BaseUpgrade::Regolith, SpecId::MERGE),
            (BaseUpgrade::Canyon, SpecId::SHANGHAI),
            (BaseUpgrade::Ecotone, SpecId::CANCUN),
            (BaseUpgrade::Fjord, SpecId::CANCUN),
            (BaseUpgrade::Granite, SpecId::CANCUN),
            (BaseUpgrade::Holocene, SpecId::CANCUN),
            (BaseUpgrade::Isthmus, SpecId::PRAGUE),
            (BaseUpgrade::Jovian, SpecId::PRAGUE),
            (BaseUpgrade::Azul, SpecId::OSAKA),
            (BaseUpgrade::Beryl, SpecId::OSAKA),
        ];

        for (base_upgrade, eth_spec) in test_cases {
            assert_eq!(base_upgrade.into_eth_spec(), eth_spec);
        }
    }

    /// Reverse lookup to find the upgrade given a chain ID and block timestamp.
    /// Returns the active upgrade at the given timestamp for the specified Base chain.
    fn upgrade_from_chain_and_timestamp(chain: Chain, timestamp: u64) -> Option<BaseUpgrade> {
        let cfg = ChainConfig::by_chain_id(chain.id())?;
        Some(upgrade_from_config_and_timestamp(cfg, timestamp))
    }

    fn upgrade_from_config_and_timestamp(cfg: &ChainConfig, timestamp: u64) -> BaseUpgrade {
        BaseUpgrade::from_timestamp(
            crate::ChainUpgrades::new(BaseUpgrade::forks_for(cfg)),
            timestamp,
        )
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
            (
                Chain::base_sepolia(),
                ChainConfig::sepolia().azul_timestamp.unwrap(),
                BaseUpgrade::Azul,
            ),
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

    #[test]
    fn test_reverse_lookup_base_specific_sequence() {
        let mut cfg = ChainConfig::mainnet().clone();
        cfg.azul_timestamp = Some(cfg.jovian_timestamp + 10);
        cfg.beryl_timestamp = Some(cfg.jovian_timestamp + 20);

        assert_eq!(
            upgrade_from_config_and_timestamp(&cfg, cfg.jovian_timestamp + 9),
            BaseUpgrade::Jovian
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(&cfg, cfg.jovian_timestamp + 10),
            BaseUpgrade::Azul
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(&cfg, cfg.jovian_timestamp + 19),
            BaseUpgrade::Azul
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(&cfg, cfg.jovian_timestamp + 20),
            BaseUpgrade::Beryl
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(&cfg, cfg.jovian_timestamp + 50),
            BaseUpgrade::Beryl
        );
    }

    #[test]
    fn test_reverse_lookup_defaults_to_beryl_after_base_thresholds() {
        let mut cfg = ChainConfig::mainnet().clone();
        cfg.azul_timestamp = Some(cfg.jovian_timestamp + 10);
        cfg.beryl_timestamp = None;

        assert_eq!(
            upgrade_from_config_and_timestamp(&cfg, cfg.jovian_timestamp + 9),
            BaseUpgrade::Jovian
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(&cfg, cfg.jovian_timestamp + 10),
            BaseUpgrade::Azul
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(&cfg, cfg.jovian_timestamp + 20),
            BaseUpgrade::Azul
        );

        cfg.azul_timestamp = None;

        assert_eq!(
            upgrade_from_config_and_timestamp(&cfg, cfg.jovian_timestamp),
            BaseUpgrade::Jovian
        );
    }
}
