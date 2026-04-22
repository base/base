//! Module containing fee parameters.

use alloy_eips::eip1559::BaseFeeParams;

/// Base Fee Config.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FeeConfig {
    /// EIP 1559 Elasticity Parameter
    #[cfg_attr(
        feature = "serde",
        serde(rename = "eip1559Elasticity", alias = "eip1559_elasticity")
    )]
    pub eip1559_elasticity: u64,
    /// EIP 1559 Denominator
    #[cfg_attr(
        feature = "serde",
        serde(rename = "eip1559Denominator", alias = "eip1559_denominator")
    )]
    pub eip1559_denominator: u64,
    /// EIP 1559 Denominator for the Canyon hardfork
    #[cfg_attr(
        feature = "serde",
        serde(rename = "eip1559DenominatorCanyon", alias = "eip1559_denominator_canyon")
    )]
    pub eip1559_denominator_canyon: u64,
}

impl FeeConfig {
    /// The Base Mainnet EIP-1559 fee parameters.
    ///
    /// These values match `base_common_chains::ChainConfig::mainnet().fee_config()`. Kept here as
    /// hardcoded constants so this crate need not depend on `base-common-chains` (which would
    /// invert the dependency direction). Drift is guarded by `mainnet_fee_config_matches_constant`.
    pub const BASE_MAINNET: Self =
        Self { eip1559_elasticity: 6, eip1559_denominator: 50, eip1559_denominator_canyon: 250 };

    /// Returns the Base Mainnet base fee config (used as serde default).
    pub const fn base_mainnet() -> Self {
        Self::BASE_MAINNET
    }

    /// Returns the [`BaseFeeParams`] before Canyon hardfork.
    pub const fn pre_canyon_params(&self) -> BaseFeeParams {
        BaseFeeParams {
            max_change_denominator: self.eip1559_denominator as u128,
            elasticity_multiplier: self.eip1559_elasticity as u128,
        }
    }

    /// Returns the [`BaseFeeParams`] since Canyon hardfork.
    pub const fn post_canyon_params(&self) -> BaseFeeParams {
        BaseFeeParams {
            max_change_denominator: self.eip1559_denominator_canyon as u128,
            elasticity_multiplier: self.eip1559_elasticity as u128,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_mainnet_pre_canyon_params() {
        let params = FeeConfig::base_mainnet().pre_canyon_params();
        assert_eq!(params.max_change_denominator, 50);
        assert_eq!(params.elasticity_multiplier, 6);
    }

    #[test]
    fn base_mainnet_post_canyon_params() {
        let params = FeeConfig::base_mainnet().post_canyon_params();
        assert_eq!(params.max_change_denominator, 250);
        assert_eq!(params.elasticity_multiplier, 6);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn base_fee_config_serde_roundtrip() {
        let raw =
            r#"{"eip1559Elasticity":6,"eip1559Denominator":50,"eip1559DenominatorCanyon":250}"#;
        let config: FeeConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(config, FeeConfig::base_mainnet());
        assert_eq!(serde_json::to_string(&config).unwrap(), raw);
    }
}
