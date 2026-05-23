//! Entry point for the `PolicyRegistry` precompile.

use base_precompile_macros::precompile;

use crate::PolicyRegistryStorage;

/// EVM entry point for the `PolicyRegistry` precompile.
#[precompile(install)]
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyRegistryPrecompile;
