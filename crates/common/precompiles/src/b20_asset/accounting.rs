//! `AssetAccounting` — storage port extension for asset tokens.

use alloc::string::String;

use alloy_primitives::U256;
use base_precompile_storage::Result;

use crate::TokenAccounting;

/// Extends [`TokenAccounting`] with asset-token-specific storage slots.
///
/// Asset identifiers (ISIN, CUSIP, etc.) are only exposed through the
/// asset-token surface, not the base B-20 surface.
pub trait AssetAccounting: TokenAccounting {
    /// Returns the current share-to-tokens ratio scaled to WAD (1e18).
    fn multiplier(&self) -> Result<U256>;
    /// Writes a new share-to-tokens ratio.
    fn set_multiplier(&mut self, ratio: U256) -> Result<()>;

    /// Returns the asset identifier value for `identifier_type`, or an empty string if unset.
    fn extra_metadata(&self, identifier_type: &str) -> Result<String>;
    /// Writes (or removes when `value` is empty) the asset identifier for `identifier_type`.
    fn set_extra_metadata_value(&mut self, identifier_type: &str, value: String) -> Result<()>;

    /// Returns `true` if `id` has been consumed by `announce`.
    fn is_announcement_id_used(&self, id: &str) -> Result<bool>;
    /// Marks `id` as consumed. Called exactly once per announcement id.
    fn mark_announcement_id_used(&mut self, id: &str) -> Result<()>;

    /// Returns the custom decimal precision stored for this asset token.
    fn decimals(&self) -> Result<u8>;
}
