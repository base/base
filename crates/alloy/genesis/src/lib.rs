#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/alloy.jpg",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![warn(
    missing_copy_implementations,
    missing_debug_implementations,
    missing_docs,
    unreachable_pub,
    clippy::missing_const_for_fn,
    rustdoc::all
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![deny(unused_must_use, rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod params;
pub use params::{
    base_fee_params, canyon_base_fee_params, BASE_SEPOLIA_BASE_FEE_PARAMS,
    BASE_SEPOLIA_CANYON_BASE_FEE_PARAMS, BASE_SEPOLIA_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER,
    OP_BASE_FEE_PARAMS, OP_CANYON_BASE_FEE_PARAMS, OP_SEPOLIA_BASE_FEE_PARAMS,
    OP_SEPOLIA_CANYON_BASE_FEE_PARAMS, OP_SEPOLIA_EIP1559_BASE_FEE_MAX_CHANGE_DENOMINATOR_CANYON,
    OP_SEPOLIA_EIP1559_DEFAULT_BASE_FEE_MAX_CHANGE_DENOMINATOR,
    OP_SEPOLIA_EIP1559_DEFAULT_ELASTICITY_MULTIPLIER,
};

pub mod addresses;
pub use addresses::AddressList;

pub mod system;
pub use system::{
    BatcherUpdateError, GasConfigUpdateError, GasLimitUpdateError, LogProcessingError,
    SystemAccounts, SystemConfig, SystemConfigUpdateError, SystemConfigUpdateType,
};

pub mod chain;
pub use chain::{ChainConfig, HardForkConfiguration, SuperchainLevel};

pub mod genesis;
pub use genesis::ChainGenesis;

pub mod rollup;
pub use rollup::{
    rollup_config_from_chain_id, RollupConfig, BASE_MAINNET_CONFIG, BASE_SEPOLIA_CONFIG,
    FJORD_MAX_SEQUENCER_DRIFT, GRANITE_CHANNEL_TIMEOUT, MAX_RLP_BYTES_PER_CHANNEL_BEDROCK,
    MAX_RLP_BYTES_PER_CHANNEL_FJORD, OP_MAINNET_CONFIG, OP_SEPOLIA_CONFIG,
};
