use alloy_hardforks::ForkCondition;

use crate::{BaseUpgrade, ChainUpgrades, Upgrades};

impl Upgrades for ChainUpgrades {
    fn fork_condition(&self, fork: BaseUpgrade) -> ForkCondition {
        self[fork]
    }
}

#[cfg(test)]
mod tests {
    use BaseUpgrade::{
        Azul, Bedrock, Beryl, Canyon, Cobalt, Denim, Ecotone, Fjord, Granite, Holocene, Isthmus,
        Jovian, Regolith, Zenith,
    };
    use alloy_hardforks::{EthereumHardfork, EthereumHardforks};

    use super::*;
    use crate::ChainConfig;

    #[test]
    fn base_mainnet_fork_conditions() {
        let base_mainnet_forks = crate::ChainConfig::mainnet().upgrades.clone();
        assert_eq!(
            base_mainnet_forks[Bedrock],
            ForkCondition::Block(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Bedrock]
                    .block_number()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_mainnet_forks[Regolith],
            ForkCondition::Timestamp(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Regolith]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_mainnet_forks[Canyon],
            ForkCondition::Timestamp(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Canyon]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_mainnet_forks[Ecotone],
            ForkCondition::Timestamp(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Ecotone]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_mainnet_forks[Fjord],
            ForkCondition::Timestamp(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Fjord]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_mainnet_forks[Granite],
            ForkCondition::Timestamp(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Granite]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_mainnet_forks[Holocene],
            ForkCondition::Timestamp(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Holocene]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_mainnet_forks[Isthmus],
            ForkCondition::Timestamp(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Isthmus]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_mainnet_forks[Jovian],
            ForkCondition::Timestamp(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Jovian]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_mainnet_forks[Azul],
            ForkCondition::Timestamp(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Azul].as_timestamp().unwrap()
            )
        );
        assert_eq!(
            base_mainnet_forks[Beryl],
            ForkCondition::Timestamp(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Beryl].as_timestamp().unwrap()
            )
        );
        assert_eq!(base_mainnet_forks[Cobalt], ForkCondition::Never);
        assert_eq!(base_mainnet_forks[Denim], ForkCondition::Never);
        assert_eq!(base_mainnet_forks[Zenith], ForkCondition::Never);
    }

    #[test]
    fn base_sepolia_fork_conditions() {
        let base_sepolia_forks = crate::ChainConfig::sepolia().upgrades.clone();
        assert_eq!(
            base_sepolia_forks[Bedrock],
            ForkCondition::Block(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Bedrock]
                    .block_number()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_sepolia_forks[Regolith],
            ForkCondition::Timestamp(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Regolith]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_sepolia_forks[Canyon],
            ForkCondition::Timestamp(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Canyon]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_sepolia_forks[Ecotone],
            ForkCondition::Timestamp(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Ecotone]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_sepolia_forks[Fjord],
            ForkCondition::Timestamp(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Fjord]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_sepolia_forks[Granite],
            ForkCondition::Timestamp(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Granite]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_sepolia_forks[Holocene],
            ForkCondition::Timestamp(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Holocene]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_sepolia_forks[Isthmus],
            ForkCondition::Timestamp(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Isthmus]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_sepolia_forks.fork_condition(Jovian),
            ForkCondition::Timestamp(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Jovian]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert_eq!(
            base_sepolia_forks[Azul],
            ForkCondition::Timestamp(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Azul].as_timestamp().unwrap()
            )
        );
        assert_eq!(
            base_sepolia_forks[Beryl],
            ForkCondition::Timestamp(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Beryl].as_timestamp().unwrap()
            )
        );
        assert_eq!(base_sepolia_forks[Cobalt], ForkCondition::Never);
        assert_eq!(base_sepolia_forks[Denim], ForkCondition::Never);
        assert_eq!(base_sepolia_forks[Zenith], ForkCondition::Never);
    }

    #[test]
    fn is_jovian_active_at_timestamp() {
        let base_mainnet_forks = crate::ChainConfig::mainnet().upgrades.clone();
        assert!(
            base_mainnet_forks.is_jovian_active_at_timestamp(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Jovian]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert!(
            !base_mainnet_forks.is_jovian_active_at_timestamp(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Jovian]
                    .as_timestamp()
                    .unwrap_or_default()
                    - 1
            )
        );
        assert!(
            base_mainnet_forks.is_jovian_active_at_timestamp(
                ChainConfig::mainnet().upgrades[crate::BaseUpgrade::Jovian]
                    .as_timestamp()
                    .unwrap_or_default()
                    + 1000
            )
        );

        let base_sepolia_forks = crate::ChainConfig::sepolia().upgrades.clone();
        assert!(
            base_sepolia_forks.is_jovian_active_at_timestamp(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Jovian]
                    .as_timestamp()
                    .unwrap_or_default()
            )
        );
        assert!(
            !base_sepolia_forks.is_jovian_active_at_timestamp(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Jovian]
                    .as_timestamp()
                    .unwrap_or_default()
                    - 1
            )
        );
        assert!(
            base_sepolia_forks.is_jovian_active_at_timestamp(
                ChainConfig::sepolia().upgrades[crate::BaseUpgrade::Jovian]
                    .as_timestamp()
                    .unwrap_or_default()
                    + 1000
            )
        );
    }

    #[test]
    fn is_azul_active_at_timestamp() {
        // Azul is scheduled on mainnet at 1779991200
        let base_mainnet_forks = crate::ChainConfig::mainnet().upgrades.clone();
        assert!(!base_mainnet_forks.is_azul_active_at_timestamp(0));
        assert!(!base_mainnet_forks.is_azul_active_at_timestamp(1_779_991_199));
        assert!(base_mainnet_forks.is_azul_active_at_timestamp(1_779_991_200));
        assert!(base_mainnet_forks.is_azul_active_at_timestamp(u64::MAX));

        // Azul is scheduled on sepolia at 1776708000
        let base_sepolia_forks = crate::ChainConfig::sepolia().upgrades.clone();
        assert!(!base_sepolia_forks.is_azul_active_at_timestamp(0));
        assert!(!base_sepolia_forks.is_azul_active_at_timestamp(1_776_707_999));
        assert!(base_sepolia_forks.is_azul_active_at_timestamp(1_776_708_000));
        assert!(base_sepolia_forks.is_azul_active_at_timestamp(u64::MAX));

        // Azul is active at genesis on devnet (ForkCondition::ZERO_TIMESTAMP)
        let devnet_forks = crate::ChainConfig::devnet().upgrades.clone();
        assert!(devnet_forks.is_azul_active_at_timestamp(0));

        // Azul is scheduled on zeronet at 1782348888
        let zeronet_forks = crate::ChainConfig::zeronet().upgrades.clone();
        assert!(!zeronet_forks.is_azul_active_at_timestamp(0));
        assert!(!zeronet_forks.is_azul_active_at_timestamp(1_782_348_887));
        assert!(zeronet_forks.is_azul_active_at_timestamp(1_782_348_888));
        assert!(zeronet_forks.is_azul_active_at_timestamp(u64::MAX));
    }

    #[test]
    fn is_beryl_active_at_timestamp() {
        let base_mainnet_forks = crate::ChainConfig::mainnet().upgrades.clone();
        assert!(!base_mainnet_forks.is_beryl_active_at_timestamp(0));
        assert!(!base_mainnet_forks.is_beryl_active_at_timestamp(1_782_410_399));
        assert!(base_mainnet_forks.is_beryl_active_at_timestamp(1_782_410_400));
        assert!(base_mainnet_forks.is_beryl_active_at_timestamp(u64::MAX));

        let base_sepolia_forks = crate::ChainConfig::sepolia().upgrades.clone();
        assert!(!base_sepolia_forks.is_beryl_active_at_timestamp(0));
        assert!(!base_sepolia_forks.is_beryl_active_at_timestamp(1_781_805_599));
        assert!(base_sepolia_forks.is_beryl_active_at_timestamp(1_781_805_600));
        assert!(base_sepolia_forks.is_beryl_active_at_timestamp(u64::MAX));

        let zeronet_forks = crate::ChainConfig::zeronet().upgrades.clone();
        assert!(!zeronet_forks.is_beryl_active_at_timestamp(0));
        assert!(!zeronet_forks.is_beryl_active_at_timestamp(1_782_349_187));
        assert!(zeronet_forks.is_beryl_active_at_timestamp(1_782_349_188));
        assert!(zeronet_forks.is_beryl_active_at_timestamp(u64::MAX));
    }

    #[test]
    fn is_zenith_active_at_timestamp() {
        let base_mainnet_forks = crate::ChainConfig::mainnet().upgrades.clone();
        assert!(!base_mainnet_forks.is_zenith_active_at_timestamp(0));
        assert!(!base_mainnet_forks.is_zenith_active_at_timestamp(u64::MAX));

        let devnet_forks = crate::ChainConfig::devnet().upgrades.clone();
        assert!(!devnet_forks.is_zenith_active_at_timestamp(0));
    }

    #[test]
    fn is_denim_active_at_timestamp() {
        // Denim is unscheduled on all built-in chains.
        let base_mainnet_forks = crate::ChainConfig::mainnet().upgrades.clone();
        assert!(!base_mainnet_forks.is_denim_active_at_timestamp(0));
        assert!(!base_mainnet_forks.is_denim_active_at_timestamp(u64::MAX));

        let devnet_forks = crate::ChainConfig::devnet().upgrades.clone();
        assert!(!devnet_forks.is_denim_active_at_timestamp(0));
        assert!(!devnet_forks.is_denim_active_at_timestamp(u64::MAX));
    }

    #[test]
    fn osaka_tracks_base_azul_activation() {
        let base_mainnet_forks = crate::ChainConfig::mainnet().upgrades.clone();
        assert_eq!(
            base_mainnet_forks.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(1_779_991_200)
        );

        let base_sepolia_forks = crate::ChainConfig::sepolia().upgrades.clone();
        assert_eq!(
            base_sepolia_forks.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(1_776_708_000)
        );

        let devnet_forks = crate::ChainConfig::devnet().upgrades.clone();
        assert_eq!(
            devnet_forks.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::ZERO_TIMESTAMP
        );

        let zeronet_forks = crate::ChainConfig::zeronet().upgrades.clone();
        assert_eq!(
            zeronet_forks.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(1_782_348_888)
        );
    }

    #[test]
    fn test_ethereum_fork_activation_consistency() {
        let base_mainnet_forks = crate::ChainConfig::mainnet().upgrades.clone();
        for ethereum_upgrade in EthereumHardfork::VARIANTS {
            let _ = base_mainnet_forks.ethereum_fork_activation(*ethereum_upgrade);
        }
        for base_upgrade in BaseUpgrade::VARIANTS {
            let _ = base_mainnet_forks.fork_condition(*base_upgrade);
        }
    }
}
