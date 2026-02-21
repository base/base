//! Module containing fee parameters.

use alloy_eips::eip1559::BaseFeeParams;

use crate::BASE_SEPOLIA_CHAIN_ID;

/// Base fee max change denominator for Base Sepolia.
pub const BASE_SEPOLIA_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER: u64 = 10;

/// Base fee max change denominator for Base Sepolia.
pub const BASE_SEPOLIA_EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR: u64 = 50;

/// Base fee max change denominator for Base Sepolia (Canyon hardfork).
pub const BASE_SEPOLIA_EIP1559_BASE_FEE_MAX_CHANGE_DENOMINATOR_CANYON: u64 = 250;

/// Base fee max change denominator for Base Mainnet.
pub const BASE_MAINNET_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER: u64 = 6;

/// Base fee max change denominator for Base Mainnet.
pub const BASE_MAINNET_EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR: u64 = 50;

/// Base fee max change denominator for Base Mainnet (Canyon hardfork).
pub const BASE_MAINNET_EIP1559_BASE_FEE_MAX_CHANGE_DENOMINATOR_CANYON: u64 = 250;

/// Get the base fee parameters for Base Mainnet.
pub const BASE_MAINNET_BASE_FEE_PARAMS: BaseFeeParams = BaseFeeParams {
    max_change_denominator: BASE_MAINNET_EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR as u128,
    elasticity_multiplier: BASE_MAINNET_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER as u128,
};

/// Get the base fee parameters for Base Sepolia.
pub const BASE_SEPOLIA_BASE_FEE_PARAMS: BaseFeeParams = BaseFeeParams {
    max_change_denominator: BASE_SEPOLIA_EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR as u128,
    elasticity_multiplier: BASE_SEPOLIA_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER as u128,
};

/// Get the base fee parameters for Base Mainnet (Canyon hardfork).
pub const BASE_MAINNET_BASE_FEE_PARAMS_CANYON: BaseFeeParams = BaseFeeParams {
    max_change_denominator: BASE_MAINNET_EIP1559_BASE_FEE_MAX_CHANGE_DENOMINATOR_CANYON as u128,
    elasticity_multiplier: BASE_MAINNET_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER as u128,
};

/// Get the base fee parameters for Base Sepolia (Canyon hardfork).
pub const BASE_SEPOLIA_BASE_FEE_PARAMS_CANYON: BaseFeeParams = BaseFeeParams {
    max_change_denominator: BASE_SEPOLIA_EIP1559_BASE_FEE_MAX_CHANGE_DENOMINATOR_CANYON as u128,
    elasticity_multiplier: BASE_SEPOLIA_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER as u128,
};

/// Returns the [`BaseFeeParams`] for the given chain id.
pub const fn base_fee_params(chain_id: u64) -> BaseFeeParams {
    match chain_id {
        BASE_SEPOLIA_CHAIN_ID => BASE_SEPOLIA_BASE_FEE_PARAMS,
        _ => BASE_MAINNET_BASE_FEE_PARAMS,
    }
}

/// Returns the [`BaseFeeParams`] for the given chain id, for Canyon hardfork.
pub const fn base_fee_params_canyon(chain_id: u64) -> BaseFeeParams {
    match chain_id {
        BASE_SEPOLIA_CHAIN_ID => BASE_SEPOLIA_BASE_FEE_PARAMS_CANYON,
        _ => BASE_MAINNET_BASE_FEE_PARAMS_CANYON,
    }
}

/// Returns the [`BaseFeeConfig`] for the given chain id.
pub const fn base_fee_config(chain_id: u64) -> BaseFeeConfig {
    match chain_id {
        BASE_SEPOLIA_CHAIN_ID => BASE_SEPOLIA_BASE_FEE_CONFIG,
        _ => BASE_MAINNET_BASE_FEE_CONFIG,
    }
}

/// Get the base fee parameters for Base Sepolia.
pub const BASE_SEPOLIA_BASE_FEE_CONFIG: BaseFeeConfig = BaseFeeConfig {
    eip1559_elasticity: BASE_SEPOLIA_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER,
    eip1559_denominator: BASE_SEPOLIA_EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR,
    eip1559_denominator_canyon: BASE_SEPOLIA_EIP1559_BASE_FEE_MAX_CHANGE_DENOMINATOR_CANYON,
};

/// Get the base fee parameters for Base Mainnet.
pub const BASE_MAINNET_BASE_FEE_CONFIG: BaseFeeConfig = BaseFeeConfig {
    eip1559_elasticity: BASE_MAINNET_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER,
    eip1559_denominator: BASE_MAINNET_EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR,
    eip1559_denominator_canyon: BASE_MAINNET_EIP1559_BASE_FEE_MAX_CHANGE_DENOMINATOR_CANYON,
};

/// Optimism Base Fee Config.
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

impl BaseFeeConfig {
    /// Get the base fee parameters for Base Mainnet
    pub const fn base_mainnet() -> Self {
        BASE_MAINNET_BASE_FEE_CONFIG
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
    use crate::BASE_MAINNET_CHAIN_ID;

    #[test]
    fn test_base_fee_params_from_chain_id() {
        assert_eq!(base_fee_params(BASE_MAINNET_CHAIN_ID), BASE_MAINNET_BASE_FEE_PARAMS);
        assert_eq!(base_fee_params(BASE_SEPOLIA_CHAIN_ID), BASE_SEPOLIA_BASE_FEE_PARAMS);
        // Unknown chain IDs fall back to Base Mainnet params
        assert_eq!(base_fee_params(0), BASE_MAINNET_BASE_FEE_PARAMS);
    }

    #[test]
    fn test_base_fee_params_canyon_from_chain_id() {
        assert_eq!(
            base_fee_params_canyon(BASE_MAINNET_CHAIN_ID),
            BASE_MAINNET_BASE_FEE_PARAMS_CANYON
        );
        assert_eq!(
            base_fee_params_canyon(BASE_SEPOLIA_CHAIN_ID),
            BASE_SEPOLIA_BASE_FEE_PARAMS_CANYON
        );
        assert_eq!(base_fee_params_canyon(0), BASE_MAINNET_BASE_FEE_PARAMS_CANYON);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_base_fee_config_ser() {
        let config = BASE_MAINNET_BASE_FEE_CONFIG;
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
        assert_eq!(config, BASE_MAINNET_BASE_FEE_CONFIG);
    }
}
