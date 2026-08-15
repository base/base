//! Precompile entry point for the EIP-8130 transaction context.

use base_precompile_macros::precompile;

use crate::TxContextStorage;

/// Entry point for the EIP-8130 transaction context precompile.
#[precompile(install)]
#[derive(Debug, Default, Clone, Copy)]
pub struct TxContext;
