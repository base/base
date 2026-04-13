//! Module containing fee parameters.

use alloy_eips::eip1559::BaseFeeParams;
use base_common_chains::BaseChainConfig;

/// Base Fee Config.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BaseFeeConfig {
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

impl From<&BaseChainConfig> for BaseFeeConfig {
    fn from(cfg: &BaseChainConfig) -> Self {
        Self {
            eip1559_elasticity: cfg.eip1559_elasticity,
            eip1559_denominator: cfg.eip1559_denominator,
            eip1559_denominator_canyon: cfg.eip1559_denominator_canyon,
        }
    }
}

impl BaseFeeConfig {
    /// Returns the [`BaseFeeConfig`] for the given chain id.
    pub fn from_chain_id(chain_id: u64) -> Self {
        let cfg = BaseChainConfig::by_chain_id(chain_id).unwrap_or(BaseChainConfig::mainnet());
        Self::from(cfg)
    }

    /// Returns the Base Mainnet base fee config (used as serde default).
    pub fn base_mainnet() -> Self {
        Self::from_chain_id(BaseChainConfig::mainnet().chain_id)
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
    fn test_base_fee_params_from_chain_id() {
        let mainnet = BaseFeeConfig::from_chain_id(BaseChainConfig::mainnet().chain_id);
        let sepolia = BaseFeeConfig::from_chain_id(BaseChainConfig::sepolia().chain_id);

        assert_eq!(
            BaseFeeConfig::from_chain_id(BaseChainConfig::mainnet().chain_id).pre_canyon_params(),
            mainnet.pre_canyon_params()
        );
        assert_eq!(
            BaseFeeConfig::from_chain_id(BaseChainConfig::sepolia().chain_id).pre_canyon_params(),
            sepolia.pre_canyon_params()
        );
        // Unknown chain IDs fall back to Base Mainnet params
        assert_eq!(
            BaseFeeConfig::from_chain_id(0).pre_canyon_params(),
            mainnet.pre_canyon_params()
        );
    }

    #[test]
    fn test_base_fee_params_canyon_from_chain_id() {
        let mainnet = BaseFeeConfig::from_chain_id(BaseChainConfig::mainnet().chain_id);
        let sepolia = BaseFeeConfig::from_chain_id(BaseChainConfig::sepolia().chain_id);

        assert_eq!(
            BaseFeeConfig::from_chain_id(BaseChainConfig::mainnet().chain_id).post_canyon_params(),
            mainnet.post_canyon_params()
        );
        assert_eq!(
            BaseFeeConfig::from_chain_id(BaseChainConfig::sepolia().chain_id).post_canyon_params(),
            sepolia.post_canyon_params()
        );
        assert_eq!(
            BaseFeeConfig::from_chain_id(0).post_canyon_params(),
            mainnet.post_canyon_params()
        );
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_base_fee_config_ser() {
        let config = BaseFeeConfig::from_chain_id(BaseChainConfig::mainnet().chain_id);
        let raw_str = serde_json::to_string(&config).unwrap();
        assert_eq!(
            raw_str,
            r#"{"eip1559Elasticity":6,"eip1559Denominator":50,"eip1559DenominatorCanyon":250}"#
        );
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_base_fee_config_deser() {
        let raw_str: &'static str =
            r#"{"eip1559Elasticity":6,"eip1559Denominator":50,"eip1559DenominatorCanyon":250}"#;
        let config: BaseFeeConfig = serde_json::from_str(raw_str).unwrap();
        assert_eq!(config, BaseFeeConfig::from_chain_id(BaseChainConfig::mainnet().chain_id));
    }
}
