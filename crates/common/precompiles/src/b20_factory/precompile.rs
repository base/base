//! Precompile entry point for the `B20Factory`.

use base_precompile_macros::precompile;

use crate::B20FactoryStorage;

/// Entry point for the `B20Factory` precompile.
#[precompile(install)]
#[derive(Debug, Default, Clone, Copy)]
pub struct B20Factory;
