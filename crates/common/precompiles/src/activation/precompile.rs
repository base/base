//! Precompile entry point for the activation registry.

use base_precompile_macros::precompile;

use super::ActivationRegistryStorage;

/// Entry point for the activation registry precompile.
#[precompile(install)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ActivationRegistry;
