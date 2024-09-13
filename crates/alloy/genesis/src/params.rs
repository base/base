//! Module containing fee parameters.

use alloy_eips::eip1559::BaseFeeParams;

/// Base fee max change denominator for Optimism Mainnet as defined in the Optimism
/// [transaction costs](https://community.optimism.io/docs/developers/build/differences/#transaction-costs) doc.
pub const OP_MAINNET_EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR: u128 = 50;

/// Base fee max change denominator for Optimism Mainnet as defined in the Optimism Canyon
/// hardfork.
pub const OP_MAINNET_EIP1559_BASE_FEE_MAX_CHANGE_DENOMINATOR_CANYON: u128 = 250;

/// Base fee max change denominator for Optimism Mainnet as defined in the Optimism
/// [transaction costs](https://community.optimism.io/docs/developers/build/differences/#transaction-costs) doc.
pub const OP_MAINNET_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER: u128 = 6;

/// Base fee max change denominator for Optimism Sepolia as defined in the Optimism
/// [transaction costs](https://community.optimism.io/docs/developers/build/differences/#transaction-costs) doc.
pub const OP_SEPOLIA_EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR: u128 = 50;

/// Base fee max change denominator for Optimism Sepolia as defined in the Optimism Canyon
/// hardfork.
pub const OP_SEPOLIA_EIP1559_BASE_FEE_MAX_CHANGE_DENOMINATOR_CANYON: u128 = 250;

/// Base fee max change denominator for Optimism Sepolia as defined in the Optimism
/// [transaction costs](https://community.optimism.io/docs/developers/build/differences/#transaction-costs) doc.
pub const OP_SEPOLIA_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER: u128 = 6;

/// Base fee max change denominator for Base Sepolia as defined in the Optimism
/// [transaction costs](https://community.optimism.io/docs/developers/build/differences/#transaction-costs) doc.
pub const BASE_SEPOLIA_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER: u128 = 10;

/// Get the base fee parameters for Optimism Sepolia.
pub const OP_SEPOLIA_BASE_FEE_PARAMS: BaseFeeParams = BaseFeeParams {
    max_change_denominator: OP_SEPOLIA_EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR,
    elasticity_multiplier: OP_SEPOLIA_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER,
};

/// Get the base fee parameters for Optimism Sepolia (post Canyon).
pub const OP_SEPOLIA_CANYON_BASE_FEE_PARAMS: BaseFeeParams = BaseFeeParams {
    max_change_denominator: OP_SEPOLIA_EIP1559_BASE_FEE_MAX_CHANGE_DENOMINATOR_CANYON,
    elasticity_multiplier: OP_SEPOLIA_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER,
};

/// Get the base fee parameters for Base Sepolia.
pub const BASE_SEPOLIA_BASE_FEE_PARAMS: BaseFeeParams = BaseFeeParams {
    max_change_denominator: OP_SEPOLIA_EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR,
    elasticity_multiplier: BASE_SEPOLIA_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER,
};

/// Get the base fee parameters for Base Sepolia (post Canyon).
pub const BASE_SEPOLIA_CANYON_BASE_FEE_PARAMS: BaseFeeParams = BaseFeeParams {
    max_change_denominator: OP_SEPOLIA_EIP1559_BASE_FEE_MAX_CHANGE_DENOMINATOR_CANYON,
    elasticity_multiplier: BASE_SEPOLIA_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER,
};

/// Get the base fee parameters for Optimism Mainnet.
pub const OP_BASE_FEE_PARAMS: BaseFeeParams = BaseFeeParams {
    max_change_denominator: OP_MAINNET_EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR,
    elasticity_multiplier: OP_MAINNET_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER,
};

/// Get the base fee parameters for Optimism Mainnet (post Canyon).
pub const OP_CANYON_BASE_FEE_PARAMS: BaseFeeParams = BaseFeeParams {
    max_change_denominator: OP_MAINNET_EIP1559_BASE_FEE_MAX_CHANGE_DENOMINATOR_CANYON,
    elasticity_multiplier: OP_MAINNET_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER,
};

/// Returns the [BaseFeeParams] for the given chain id.
pub const fn base_fee_params(chain_id: u64) -> BaseFeeParams {
    match chain_id {
        10 => OP_BASE_FEE_PARAMS,
        11155420 => OP_SEPOLIA_BASE_FEE_PARAMS,
        8453 => OP_BASE_FEE_PARAMS,
        84532 => BASE_SEPOLIA_BASE_FEE_PARAMS,
        _ => OP_BASE_FEE_PARAMS,
    }
}

/// Returns the Canyon [BaseFeeParams] for the given chain id.
pub const fn canyon_base_fee_params(chain_id: u64) -> BaseFeeParams {
    match chain_id {
        10 => OP_CANYON_BASE_FEE_PARAMS,
        11155420 => OP_SEPOLIA_CANYON_BASE_FEE_PARAMS,
        8453 => OP_CANYON_BASE_FEE_PARAMS,
        84532 => BASE_SEPOLIA_CANYON_BASE_FEE_PARAMS,
        _ => OP_CANYON_BASE_FEE_PARAMS,
    }
}
