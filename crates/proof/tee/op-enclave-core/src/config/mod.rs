//! Configuration module for chain and rollup configuration.
//!
//! This module provides default rollup configuration that matches
//! the Go implementation's `DefaultDeployConfig()`.

mod defaults;

pub use defaults::{
    default_l1_config, default_rollup_config, l1_config_for_l2_chain_id, sepolia_l1_config,
};
