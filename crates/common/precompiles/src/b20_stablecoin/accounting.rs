//! `StablecoinAccounting` — storage port extension for stablecoin tokens.

use alloc::string::String;

use base_precompile_storage::Result;

use crate::TokenAccounting;

/// Extends [`TokenAccounting`] with the stablecoin-specific `currency` slot.
///
/// Only [`super::B20StablecoinToken`] requires this bound; default and asset
/// tokens use the base [`TokenAccounting`] port exclusively.
pub trait StablecoinAccounting: TokenAccounting {
    /// Returns the stablecoin currency identifier.
    fn currency(&self) -> Result<String>;
}
