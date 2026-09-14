use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};

/// Fork schedule for Base's supported Engine API payload formats.
///
/// Denim adopts Amsterdam EVM semantics, but does not yet introduce Ethereum's
/// Amsterdam payload attributes, BAL transport, or Engine API method versions.
/// Keep those wire-format checks separate from the execution fork schedule.
/// This view must not be used to configure EVM execution or consensus validation.
#[derive(Debug, Clone)]
pub struct BaseEngineApiForks<T>(pub T);

impl<T: EthereumHardforks> EthereumHardforks for BaseEngineApiForks<T> {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        match fork {
            EthereumHardfork::Amsterdam => ForkCondition::Never,
            _ => self.0.ethereum_fork_activation(fork),
        }
    }
}

#[cfg(test)]
mod tests {
    use base_common_chains::ChainUpgrades;
    use base_common_genesis::BaseUpgrade;

    use super::*;

    #[test]
    fn wire_forks_preserve_earlier_activations_without_disabling_amsterdam_execution() {
        let execution = ChainUpgrades::new([
            (BaseUpgrade::Ecotone, ForkCondition::Timestamp(10)),
            (BaseUpgrade::Isthmus, ForkCondition::Timestamp(20)),
            (BaseUpgrade::Azul, ForkCondition::Timestamp(30)),
            (BaseUpgrade::Denim, ForkCondition::Timestamp(40)),
        ]);
        let wire = BaseEngineApiForks(&execution);

        for fork in [EthereumHardfork::Cancun, EthereumHardfork::Prague, EthereumHardfork::Osaka] {
            let condition = execution.ethereum_fork_activation(fork);
            assert_eq!(wire.ethereum_fork_activation(fork), condition);
        }
        assert!(!execution.is_amsterdam_active_at_timestamp(39));
        assert!(execution.is_amsterdam_active_at_timestamp(40));
        assert!(!wire.is_amsterdam_active_at_timestamp(u64::MAX));
    }
}
