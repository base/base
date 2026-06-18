use alloy_hardforks::ForkCondition;
pub use base_common_genesis::BaseUpgrade;
use revm::primitives::hardfork::SpecId;

use crate::{ChainConfig, Upgrades};

/// Execution-layer extension methods for [`BaseUpgrade`].
///
/// [`BaseUpgrade`] is defined in `base-common-genesis`, which cannot depend on `revm` or
/// [`ChainConfig`]. These helpers, which map upgrades onto revm specs and Base chain schedules,
/// therefore live here in `base-common-chains`. Bring this trait into scope to call them.
pub trait BaseUpgradeExt: Sized {
    /// Converts the Base upgrade into its matching Ethereum execution spec.
    ///
    /// The contract-only upgrades inherit the execution spec of the surrounding era: `Delta`
    /// behaves like `Canyon` (Shanghai) and `PectraBlobSchedule` like `Holocene` (Cancun).
    fn into_eth_spec(self) -> SpecId;

    /// Returns the execution fork ladder with activation conditions for the given chain config.
    fn forks_for(cfg: &ChainConfig) -> [(BaseUpgrade, ForkCondition); 12];

    /// Base mainnet list of execution upgrades.
    fn mainnet() -> [(BaseUpgrade, ForkCondition); 12] {
        Self::forks_for(ChainConfig::mainnet())
    }

    /// Base Sepolia list of execution upgrades.
    fn sepolia() -> [(BaseUpgrade, ForkCondition); 12] {
        Self::forks_for(ChainConfig::sepolia())
    }

    /// Devnet list of execution upgrades.
    fn devnet() -> [(BaseUpgrade, ForkCondition); 12] {
        Self::forks_for(ChainConfig::devnet())
    }

    /// Base Zeronet list of execution upgrades.
    fn zeronet() -> [(BaseUpgrade, ForkCondition); 12] {
        Self::forks_for(ChainConfig::zeronet())
    }

    /// Returns the active Base upgrade at the given timestamp.
    ///
    /// This is intended for post-Bedrock timestamp-based fork resolution.
    fn from_timestamp(chain_spec: impl Upgrades, timestamp: u64) -> BaseUpgrade;
}

impl BaseUpgradeExt for BaseUpgrade {
    fn into_eth_spec(self) -> SpecId {
        match self {
            Self::Bedrock | Self::Regolith => SpecId::MERGE,
            Self::Canyon | Self::Delta => SpecId::SHANGHAI,
            Self::Ecotone
            | Self::Fjord
            | Self::Granite
            | Self::Holocene
            | Self::PectraBlobSchedule => SpecId::CANCUN,
            Self::Isthmus | Self::Jovian => SpecId::PRAGUE,
            // Azul, Beryl, Cobalt, and newer Base upgrades inherit the latest known Ethereum spec
            // until explicitly mapped.
            _ => SpecId::OSAKA,
        }
    }

    fn forks_for(cfg: &ChainConfig) -> [(BaseUpgrade, ForkCondition); 12] {
        let azul = cfg.azul_timestamp.map_or(ForkCondition::Never, ForkCondition::Timestamp);
        let beryl = cfg.beryl_timestamp.map_or(ForkCondition::Never, ForkCondition::Timestamp);
        let cobalt = cfg.cobalt_timestamp.map_or(ForkCondition::Never, ForkCondition::Timestamp);
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
            (Self::Cobalt, cobalt),
        ]
    }

    fn from_timestamp(chain_spec: impl Upgrades, timestamp: u64) -> BaseUpgrade {
        if chain_spec.is_cobalt_active_at_timestamp(timestamp) {
            Self::Cobalt
        } else if chain_spec.is_beryl_active_at_timestamp(timestamp) {
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
}

#[cfg(test)]
mod tests {
    use core::str::FromStr;

    use alloy_chains::Chain;
    use alloy_hardforks::EthereumHardfork;

    use super::*;

    extern crate alloc;

    #[test]
    fn check_base_upgrade_from_str() {
        let upgrade_str = [
            "beDrOck", "rEgOlITH", "cAnYoN", "eCoToNe", "FJorD", "GRaNiTe", "hOlOcEnE", "isthMUS",
            "jOvIaN", "aZuL", "bErYl", "cObAlT",
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
            BaseUpgrade::Cobalt,
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
    fn ethereum_hardforks_map_to_base_upgrades() {
        assert_eq!(
            BaseUpgrade::from_ethereum_hardfork(EthereumHardfork::Shanghai),
            Some(BaseUpgrade::Canyon)
        );
        assert_eq!(
            BaseUpgrade::from_ethereum_hardfork(EthereumHardfork::Cancun),
            Some(BaseUpgrade::Ecotone)
        );
        assert_eq!(
            BaseUpgrade::from_ethereum_hardfork(EthereumHardfork::Prague),
            Some(BaseUpgrade::Isthmus)
        );
        assert_eq!(
            BaseUpgrade::from_ethereum_hardfork(EthereumHardfork::Osaka),
            Some(BaseUpgrade::Azul)
        );
        assert_eq!(BaseUpgrade::from_ethereum_hardfork(EthereumHardfork::London), None);
    }

    #[test]
    fn contract_upgrade_aliases_resolve_consistently() {
        let aliases = [
            (EthereumHardfork::Shanghai.name(), BaseUpgrade::Canyon),
            (EthereumHardfork::Cancun.name(), BaseUpgrade::Ecotone),
            (EthereumHardfork::Prague.name(), BaseUpgrade::Isthmus),
            (EthereumHardfork::Osaka.name(), BaseUpgrade::Azul),
            (BaseUpgrade::Beryl.name(), BaseUpgrade::Beryl),
            ("v1", BaseUpgrade::Azul),
            ("v2", BaseUpgrade::Beryl),
            ("v3", BaseUpgrade::Cobalt),
        ];

        for (alias, upgrade) in aliases {
            assert_eq!(BaseUpgrade::from_contract_fork_name(alias), Some(upgrade));
        }
    }

    #[test]
    fn fork_names_are_trimmed_and_case_insensitive() {
        assert_eq!(BaseUpgrade::from_contract_fork_name("  shAnGhAi  "), Some(BaseUpgrade::Canyon));
        assert_eq!(BaseUpgrade::from_contract_fork_name("\tbase_azul\n"), Some(BaseUpgrade::Azul));
        assert_eq!(BaseUpgrade::from_contract_fork_name("\n bERyl\t"), Some(BaseUpgrade::Beryl));
    }

    #[test]
    fn check_base_upgrade_eth_spec_mapping() {
        let test_cases = [
            (BaseUpgrade::Bedrock, SpecId::MERGE),
            (BaseUpgrade::Regolith, SpecId::MERGE),
            (BaseUpgrade::Canyon, SpecId::SHANGHAI),
            (BaseUpgrade::Delta, SpecId::SHANGHAI),
            (BaseUpgrade::Ecotone, SpecId::CANCUN),
            (BaseUpgrade::Fjord, SpecId::CANCUN),
            (BaseUpgrade::Granite, SpecId::CANCUN),
            (BaseUpgrade::Holocene, SpecId::CANCUN),
            (BaseUpgrade::PectraBlobSchedule, SpecId::CANCUN),
            (BaseUpgrade::Isthmus, SpecId::PRAGUE),
            (BaseUpgrade::Jovian, SpecId::PRAGUE),
            (BaseUpgrade::Azul, SpecId::OSAKA),
            (BaseUpgrade::Beryl, SpecId::OSAKA),
            (BaseUpgrade::Cobalt, SpecId::OSAKA),
        ];

        for (base_upgrade, eth_spec) in test_cases {
            assert_eq!(base_upgrade.into_eth_spec(), eth_spec);
        }
    }

    #[test]
    fn contract_upgrade_parses_aliases() {
        assert_eq!(BaseUpgrade::from_contract_fork_name("base_azul"), Some(BaseUpgrade::Azul));
        assert_eq!(BaseUpgrade::from_contract_fork_name("shanghai"), Some(BaseUpgrade::Canyon));
        assert_eq!(
            BaseUpgrade::from_contract_fork_name("pectra_blob_schedule"),
            Some(BaseUpgrade::PectraBlobSchedule)
        );
    }

    #[test]
    fn bedrock_is_not_contract_backed() {
        // Bedrock is block-activated and never signaled by the L1 contract.
        assert!(!BaseUpgrade::Bedrock.is_contract_backed());
        assert_eq!(BaseUpgrade::from_contract_fork_name("bedrock"), None);
        assert!(!BaseUpgrade::CONTRACT_VARIANTS.contains(&BaseUpgrade::Bedrock));
    }

    #[test]
    fn contract_only_upgrades_are_absent_from_execution_ladder() {
        // Delta and PectraBlobSchedule are contract-backed config upgrades that do not change
        // EVM execution, so they have no execution index and are excluded from the ladder.
        for upgrade in [BaseUpgrade::Delta, BaseUpgrade::PectraBlobSchedule] {
            assert!(!upgrade.is_execution());
            assert_eq!(upgrade.execution_idx(), None);
            assert!(upgrade.is_contract_backed());
            assert!(!BaseUpgrade::EXECUTION_VARIANTS.contains(&upgrade));
            assert!(BaseUpgrade::CONTRACT_VARIANTS.contains(&upgrade));
        }
    }

    #[test]
    fn contract_upgrade_tracks_execution_companions() {
        assert_eq!(BaseUpgrade::Canyon.execution_hardfork(), Some(EthereumHardfork::Shanghai));
        assert_eq!(BaseUpgrade::Regolith.execution_hardfork(), None);
        assert_eq!(BaseUpgrade::Delta.execution_hardfork(), None);
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
                Chain::base_mainnet(),
                ChainConfig::mainnet().beryl_timestamp.unwrap(),
                BaseUpgrade::Beryl,
            ),
            (
                Chain::base_sepolia(),
                ChainConfig::sepolia().azul_timestamp.unwrap(),
                BaseUpgrade::Azul,
            ),
            (
                Chain::base_sepolia(),
                ChainConfig::sepolia().beryl_timestamp.unwrap(),
                BaseUpgrade::Beryl,
            ),
            (
                Chain::from_id(ChainConfig::zeronet().chain_id),
                ChainConfig::zeronet().beryl_timestamp.unwrap(),
                BaseUpgrade::Beryl,
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
        cfg.cobalt_timestamp = Some(cfg.jovian_timestamp + 30);

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
            upgrade_from_config_and_timestamp(&cfg, cfg.jovian_timestamp + 29),
            BaseUpgrade::Beryl
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(&cfg, cfg.jovian_timestamp + 30),
            BaseUpgrade::Cobalt
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(&cfg, cfg.jovian_timestamp + 50),
            BaseUpgrade::Cobalt
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
