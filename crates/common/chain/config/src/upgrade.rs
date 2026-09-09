use alloy_hardforks::ForkCondition;

use crate::{BaseUpgrade, ChainConfig, Upgrades};

/// Chain schedule queries for [`BaseUpgrade`].
///
/// EVM spec selection belongs to the execution layer; this interface only resolves
/// configured upgrade activation.
impl BaseUpgrade {
    /// Returns the execution fork ladder with activation conditions for the given chain config.
    pub fn forks_for(cfg: &ChainConfig) -> [(BaseUpgrade, ForkCondition); 13] {
        BaseUpgrade::EXECUTION_VARIANTS.map(|fork| (fork, cfg.upgrades[fork]))
    }

    /// Base mainnet list of execution upgrades.
    pub fn mainnet() -> [(BaseUpgrade, ForkCondition); 13] {
        Self::forks_for(ChainConfig::mainnet())
    }

    /// Base Sepolia list of execution upgrades.
    pub fn sepolia() -> [(BaseUpgrade, ForkCondition); 13] {
        Self::forks_for(ChainConfig::sepolia())
    }

    /// Devnet list of execution upgrades.
    pub fn devnet() -> [(BaseUpgrade, ForkCondition); 13] {
        Self::forks_for(ChainConfig::devnet())
    }

    /// Base Zeronet list of execution upgrades.
    pub fn zeronet() -> [(BaseUpgrade, ForkCondition); 13] {
        Self::forks_for(ChainConfig::zeronet())
    }

    /// Returns the active Base upgrade at the given timestamp.
    ///
    /// This is intended for post-Bedrock timestamp-based fork resolution.
    pub fn from_timestamp(chain_spec: impl Upgrades, timestamp: u64) -> BaseUpgrade {
        if chain_spec.is_denim_active_at_timestamp(timestamp) {
            Self::Denim
        } else if chain_spec.is_cobalt_active_at_timestamp(timestamp) {
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
            "jOvIaN", "aZuL", "bErYl", "cObAlT", "dEnIm", "zEnItH",
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
            BaseUpgrade::Denim,
            BaseUpgrade::Zenith,
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
        assert_eq!(BaseUpgrade::LATEST, BaseUpgrade::Beryl);
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
    fn denim_is_contract_backed_and_in_execution_ladder() {
        // Denim is a first-class upgrade: schedulable via the L1 upgrade signal on live chains
        // and part of the execution fork ladder, but unscheduled by default.
        assert!(BaseUpgrade::Denim.is_contract_backed());
        assert!(BaseUpgrade::Denim.is_execution());
        assert_eq!(BaseUpgrade::from_contract_fork_name("denim"), Some(BaseUpgrade::Denim));
        assert!(BaseUpgrade::CONTRACT_VARIANTS.contains(&BaseUpgrade::Denim));
        assert!(BaseUpgrade::EXECUTION_VARIANTS.contains(&BaseUpgrade::Denim));
    }

    #[test]
    fn zenith_is_genesis_only_not_l1_signal_backed() {
        // Zenith activates via genesis config only: it is not contract-backed, not signalable
        // via the L1 contract, and absent from both the contract-backed and execution sets.
        assert!(!BaseUpgrade::Zenith.is_contract_backed());
        assert_eq!(BaseUpgrade::from_contract_fork_name("zenith"), None);
        assert!(!BaseUpgrade::CONTRACT_VARIANTS.contains(&BaseUpgrade::Zenith));
        assert!(!BaseUpgrade::EXECUTION_VARIANTS.contains(&BaseUpgrade::Zenith));
    }

    #[test]
    fn contract_only_upgrades_are_absent_from_execution_ladder() {
        // Delta and PectraBlobSchedule are contract-backed config upgrades that do not
        // change EVM execution, so they have no execution index and are excluded from the ladder.
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
            (
                Chain::base_mainnet(),
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Canyon]
                    .as_timestamp()
                    .unwrap_or_default(),
                BaseUpgrade::Canyon,
            ),
            (
                Chain::base_mainnet(),
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Ecotone]
                    .as_timestamp()
                    .unwrap_or_default(),
                BaseUpgrade::Ecotone,
            ),
            (
                Chain::base_mainnet(),
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Jovian]
                    .as_timestamp()
                    .unwrap_or_default(),
                BaseUpgrade::Jovian,
            ),
            (
                Chain::base_sepolia(),
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Canyon]
                    .as_timestamp()
                    .unwrap_or_default(),
                BaseUpgrade::Canyon,
            ),
            (
                Chain::base_sepolia(),
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Ecotone]
                    .as_timestamp()
                    .unwrap_or_default(),
                BaseUpgrade::Ecotone,
            ),
            (
                Chain::base_sepolia(),
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Jovian]
                    .as_timestamp()
                    .unwrap_or_default(),
                BaseUpgrade::Jovian,
            ),
            (
                Chain::base_mainnet(),
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Beryl].as_timestamp().unwrap(),
                BaseUpgrade::Beryl,
            ),
            (
                Chain::base_sepolia(),
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Azul].as_timestamp().unwrap(),
                BaseUpgrade::Azul,
            ),
            (
                Chain::base_sepolia(),
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Beryl].as_timestamp().unwrap(),
                BaseUpgrade::Beryl,
            ),
            (
                Chain::from_id(ChainConfig::zeronet().chain_id),
                ChainConfig::zeronet().upgrades[crate::BaseUpgrade::Beryl].as_timestamp().unwrap(),
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
        cfg.upgrades.insert(
            crate::BaseUpgrade::Azul,
            ForkCondition::Timestamp(
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 10,
            ),
        );
        cfg.upgrades.insert(
            crate::BaseUpgrade::Beryl,
            ForkCondition::Timestamp(
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 20,
            ),
        );
        cfg.upgrades.insert(
            crate::BaseUpgrade::Cobalt,
            ForkCondition::Timestamp(
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 30,
            ),
        );

        assert_eq!(
            upgrade_from_config_and_timestamp(
                &cfg,
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 9
            ),
            BaseUpgrade::Jovian
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(
                &cfg,
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 10
            ),
            BaseUpgrade::Azul
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(
                &cfg,
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 19
            ),
            BaseUpgrade::Azul
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(
                &cfg,
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 20
            ),
            BaseUpgrade::Beryl
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(
                &cfg,
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 29
            ),
            BaseUpgrade::Beryl
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(
                &cfg,
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 30
            ),
            BaseUpgrade::Cobalt
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(
                &cfg,
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 50
            ),
            BaseUpgrade::Cobalt
        );
    }

    #[test]
    fn test_reverse_lookup_defaults_to_beryl_after_base_thresholds() {
        let mut cfg = ChainConfig::mainnet().clone();
        cfg.upgrades.insert(
            crate::BaseUpgrade::Azul,
            ForkCondition::Timestamp(
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 10,
            ),
        );
        cfg.upgrades.insert(crate::BaseUpgrade::Beryl, ForkCondition::Never);

        assert_eq!(
            upgrade_from_config_and_timestamp(
                &cfg,
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 9
            ),
            BaseUpgrade::Jovian
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(
                &cfg,
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 10
            ),
            BaseUpgrade::Azul
        );
        assert_eq!(
            upgrade_from_config_and_timestamp(
                &cfg,
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default() + 20
            ),
            BaseUpgrade::Azul
        );

        cfg.upgrades.insert(crate::BaseUpgrade::Azul, ForkCondition::Never);

        assert_eq!(
            upgrade_from_config_and_timestamp(
                &cfg,
                cfg.upgrades[crate::BaseUpgrade::Jovian].as_timestamp().unwrap_or_default()
            ),
            BaseUpgrade::Jovian
        );
    }
}
