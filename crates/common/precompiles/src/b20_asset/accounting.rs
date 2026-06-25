//! `AssetAccounting` — storage port extension for asset tokens.

use alloc::string::String;

use alloy_primitives::U256;
use base_precompile_storage::Result;

use crate::TokenAccounting;

/// Extends [`TokenAccounting`] with asset-token-specific storage slots.
///
/// Extra metadata entries are only exposed through the asset-token surface,
/// not the base B-20 surface.
pub trait AssetAccounting: TokenAccounting {
    /// Returns the current multiplier scaled to WAD (1e18).
    fn multiplier(&self) -> Result<U256>;
    /// Writes a new multiplier.
    fn set_multiplier(&mut self, multiplier: U256) -> Result<()>;

    /// Returns the extra-metadata value for `key`, or an empty string if unset.
    fn extra_metadata(&self, key: &str) -> Result<String>;
    /// Writes (or removes when `value` is empty) the extra-metadata entry for `key`.
    fn set_extra_metadata_value(&mut self, key: &str, value: String) -> Result<()>;

    /// Returns `true` if `id` has been consumed by `announce`.
    fn is_announcement_id_used(&self, id: &str) -> Result<bool>;
    /// Marks `id` as consumed. Called exactly once per announcement id.
    fn mark_announcement_id_used(&mut self, id: &str) -> Result<()>;

    /// Returns the custom decimal precision stored for this asset token.
    fn decimals(&self) -> Result<u8>;
}
