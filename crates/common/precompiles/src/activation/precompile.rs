//! Precompile entry point for the activation registry.

use alloy_primitives::Address;
use base_precompile_macros::precompile;

use super::ActivationRegistryStorage;

/// Entry point for the activation registry precompile.
#[precompile(install, args(activation_admin_address: Option<Address>))]
#[derive(Debug, Default, Clone, Copy)]
pub struct ActivationRegistry;
