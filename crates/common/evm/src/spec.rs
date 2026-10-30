//! Contains the `[BaseSpecId]` type and its implementation.

use alloy_consensus::BlockHeader;
use base_common_chains::{BaseUpgradeExt, Upgrades};
use base_common_genesis::{BaseFeature, BaseUpgrade};
use revm::primitives::hardfork::SpecId;

/// EVM-facing Base spec id.
///
/// This wraps the canonical Base upgrade type and adds revm-specific behavior.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct BaseSpecId(BaseUpgrade);

impl BaseSpecId {
    /// Creates a new Base EVM spec id for the given Base upgrade.
    pub const fn new(upgrade: BaseUpgrade) -> Self {
        Self(upgrade)
    }

    /// Returns the wrapped Base upgrade.
    pub const fn upgrade(self) -> BaseUpgrade {
        self.0
    }

    /// Converts the [`BaseSpecId`] into a [`SpecId`].
    pub fn into_eth_spec(self) -> SpecId {
        self.0.into_eth_spec()
    }

    /// Checks if the given Base upgrade is enabled in this spec.
    pub const fn is_enabled_in(self, other: BaseUpgrade) -> bool {
        other as u8 <= self.0 as u8
    }

    /// Whether this spec includes a behavior, preserving the execution upgrade ordering.
    pub const fn is_feature_enabled(self, feature: BaseFeature) -> bool {
        self.0.is_feature_enabled(feature)
    }

    /// Parses the [`BaseSpecId`] from the chain spec and block header.
    pub fn from_header(chain_spec: impl Upgrades, header: impl BlockHeader) -> Self {
        Self::from_timestamp(chain_spec, header.timestamp())
    }

    /// Returns the [`BaseSpecId`] at the given timestamp.
    ///
    /// # Note
    ///
    /// This is only intended to be used after Bedrock, when upgrades are activated by
    /// timestamp.
    pub fn from_timestamp(chain_spec: impl Upgrades, timestamp: u64) -> Self {
        Self(BaseUpgrade::from_timestamp(chain_spec, timestamp))
    }
}

impl From<BaseUpgrade> for BaseSpecId {
    fn from(upgrade: BaseUpgrade) -> Self {
        Self(upgrade)
    }
}

impl From<BaseSpecId> for SpecId {
    fn from(spec: BaseSpecId) -> Self {
        spec.into_eth_spec()
    }
}

impl From<BaseSpecId> for BaseUpgrade {
    fn from(spec: BaseSpecId) -> Self {
        spec.upgrade()
    }
}

impl From<BaseSpecId> for &'static str {
    fn from(spec: BaseSpecId) -> Self {
        spec.0.name()
    }
}

impl core::fmt::Display for BaseSpecId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

impl core::str::FromStr for BaseSpecId {
    type Err = <BaseUpgrade as core::str::FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<BaseUpgrade>().map(Self)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use base_common_chains::ChainUpgrades;
    use base_common_genesis::RollupConfig;

    use super::*;

    #[test]
    fn test_base_spec_id_eth_spec_compatibility() {
        // Define test cases: (BaseUpgrade, enabled in ETH specs, enabled in Base upgrades)
        let test_cases = [
            (
                BaseUpgrade::Bedrock,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, false),
                    (SpecId::CANCUN, false),
                    (SpecId::default(), false),
                ],
                vec![(BaseUpgrade::Bedrock, true), (BaseUpgrade::Regolith, false)],
            ),
            (
                BaseUpgrade::Regolith,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, false),
                    (SpecId::CANCUN, false),
                    (SpecId::default(), false),
                ],
                vec![(BaseUpgrade::Bedrock, true), (BaseUpgrade::Regolith, true)],
            ),
            (
                BaseUpgrade::Canyon,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, false),
                    (SpecId::default(), false),
                ],
                vec![
                    (BaseUpgrade::Bedrock, true),
                    (BaseUpgrade::Regolith, true),
                    (BaseUpgrade::Canyon, true),
                ],
            ),
            (
                BaseUpgrade::Ecotone,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::default(), false),
                ],
                vec![
                    (BaseUpgrade::Bedrock, true),
                    (BaseUpgrade::Regolith, true),
                    (BaseUpgrade::Canyon, true),
                    (BaseUpgrade::Ecotone, true),
                ],
            ),
            (
                BaseUpgrade::Fjord,
                vec![
                    (SpecId::MERGE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::default(), false),
                ],
                vec![
                    (BaseUpgrade::Bedrock, true),
                    (BaseUpgrade::Regolith, true),
                    (BaseUpgrade::Canyon, true),
                    (BaseUpgrade::Ecotone, true),
                    (BaseUpgrade::Fjord, true),
                ],
            ),
            (
                BaseUpgrade::Jovian,
                vec![
                    (SpecId::PRAGUE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::MERGE, true),
                ],
                vec![
                    (BaseUpgrade::Bedrock, true),
                    (BaseUpgrade::Regolith, true),
                    (BaseUpgrade::Canyon, true),
                    (BaseUpgrade::Ecotone, true),
                    (BaseUpgrade::Fjord, true),
                    (BaseUpgrade::Holocene, true),
                    (BaseUpgrade::Isthmus, true),
                ],
            ),
            (
                BaseUpgrade::Azul,
                vec![
                    (SpecId::OSAKA, true),
                    (SpecId::PRAGUE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::MERGE, true),
                ],
                vec![
                    (BaseUpgrade::Bedrock, true),
                    (BaseUpgrade::Regolith, true),
                    (BaseUpgrade::Canyon, true),
                    (BaseUpgrade::Ecotone, true),
                    (BaseUpgrade::Fjord, true),
                    (BaseUpgrade::Holocene, true),
                    (BaseUpgrade::Isthmus, true),
                    (BaseUpgrade::Jovian, true),
                ],
            ),
            (
                BaseUpgrade::Beryl,
                vec![
                    (SpecId::OSAKA, true),
                    (SpecId::PRAGUE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::MERGE, true),
                ],
                vec![
                    (BaseUpgrade::Bedrock, true),
                    (BaseUpgrade::Regolith, true),
                    (BaseUpgrade::Canyon, true),
                    (BaseUpgrade::Ecotone, true),
                    (BaseUpgrade::Fjord, true),
                    (BaseUpgrade::Holocene, true),
                    (BaseUpgrade::Isthmus, true),
                    (BaseUpgrade::Jovian, true),
                    (BaseUpgrade::Azul, true),
                ],
            ),
            (
                BaseUpgrade::Cobalt,
                vec![
                    (SpecId::OSAKA, true),
                    (SpecId::PRAGUE, true),
                    (SpecId::SHANGHAI, true),
                    (SpecId::CANCUN, true),
                    (SpecId::MERGE, true),
                ],
                vec![
                    (BaseUpgrade::Bedrock, true),
                    (BaseUpgrade::Regolith, true),
                    (BaseUpgrade::Canyon, true),
                    (BaseUpgrade::Ecotone, true),
                    (BaseUpgrade::Fjord, true),
                    (BaseUpgrade::Holocene, true),
                    (BaseUpgrade::Isthmus, true),
                    (BaseUpgrade::Jovian, true),
                    (BaseUpgrade::Azul, true),
                    (BaseUpgrade::Beryl, true),
                ],
            ),
        ];

        for (base_upgrade, eth_tests, base_tests) in test_cases {
            let base_spec = BaseSpecId::new(base_upgrade);

            // Test ETH spec compatibility
            for (eth_spec, expected) in eth_tests {
                assert_eq!(
                    base_spec.into_eth_spec().is_enabled_in(eth_spec),
                    expected,
                    "{base_spec:?} should {} be enabled in ETH {eth_spec:?}",
                    if expected { "" } else { "not " },
                );
            }

            // Test Base upgrade compatibility
            for (other_base_upgrade, expected) in base_tests {
                assert_eq!(
                    base_spec.is_enabled_in(other_base_upgrade),
                    expected,
                    "{base_spec:?} should {} be enabled in Base {other_base_upgrade:?}",
                    if expected { "" } else { "not " },
                );
            }
        }
    }

    #[test]
    fn default_base_spec_id() {
        assert_eq!(BaseSpecId::default().upgrade(), BaseUpgrade::LATEST);
    }

    #[test]
    fn semantic_features_preserve_execution_order_including_contract_only_specs() {
        let cases = [
            (BaseFeature::DepositNonce, BaseUpgrade::Regolith),
            (BaseFeature::DepositReceiptVersion, BaseUpgrade::Canyon),
            (BaseFeature::BlobAwareL1Fees, BaseUpgrade::Ecotone),
            (BaseFeature::FastLzL1Fees, BaseUpgrade::Fjord),
            (BaseFeature::OperatorFee, BaseUpgrade::Isthmus),
            (BaseFeature::DaFootprint, BaseUpgrade::Jovian),
            (BaseFeature::DynamicStorageTailCleanup, BaseUpgrade::Cobalt),
            (BaseFeature::SubsecondBlocks, BaseUpgrade::Denim),
            (BaseFeature::Eip8130, BaseUpgrade::Everest),
            (BaseFeature::Keystore, BaseUpgrade::Zenith),
        ];
        for upgrade in BaseUpgrade::EXECUTION_VARIANTS.into_iter().chain([
            BaseUpgrade::Delta,
            BaseUpgrade::PectraBlobSchedule,
            BaseUpgrade::Zenith,
        ]) {
            let spec = BaseSpecId::new(upgrade);
            for (feature, activation) in cases {
                assert_eq!(
                    spec.is_feature_enabled(feature),
                    spec.is_enabled_in(activation),
                    "{feature:?} at {upgrade:?}"
                );
            }
        }
    }

    #[test]
    fn gas_limit_billing_requires_genesis_configured_zenith() {
        // Every scheduled execution upgrade predates gas-limit billing; only Zenith enables it.
        for upgrade in BaseUpgrade::EXECUTION_VARIANTS {
            let spec = BaseSpecId::new(upgrade);
            assert!(!spec.is_feature_enabled(BaseFeature::GasLimitBilling), "{upgrade:?}");
        }
        assert!(
            BaseSpecId::new(BaseUpgrade::Zenith).is_feature_enabled(BaseFeature::GasLimitBilling)
        );

        for chain in [ChainUpgrades::mainnet(), ChainUpgrades::sepolia()] {
            let spec = BaseSpecId::from_timestamp(&chain, u64::MAX);
            assert!(!spec.is_feature_enabled(BaseFeature::GasLimitBilling), "{spec:?}");
        }

        const ZENITH_TIME: u64 = 42;
        let mut cfg = RollupConfig::default();
        cfg.upgrades.base.everest = Some(0);
        let spec = BaseSpecId::from_timestamp(&cfg, u64::MAX);
        assert!(!spec.is_feature_enabled(BaseFeature::GasLimitBilling));

        cfg.upgrades.base.zenith = Some(ZENITH_TIME);
        let before = BaseSpecId::from_timestamp(&cfg, ZENITH_TIME - 1);
        let at = BaseSpecId::from_timestamp(&cfg, ZENITH_TIME);
        assert!(!before.is_feature_enabled(BaseFeature::GasLimitBilling));
        assert!(at.is_feature_enabled(BaseFeature::GasLimitBilling));
    }
}
