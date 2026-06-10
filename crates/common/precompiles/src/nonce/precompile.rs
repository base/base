//! Precompile entry point for the EIP-8130 2D nonce manager.

use base_precompile_macros::precompile;

use crate::NonceManagerStorage;

/// Entry point for the EIP-8130 2D nonce manager precompile.
#[precompile(install)]
#[derive(Debug, Default, Clone, Copy)]
pub struct NonceManager;
