//! `SecurityAccounting` — storage port extension for security tokens.

use alloc::string::String;

use alloy_primitives::U256;
use base_precompile_storage::Result;

use crate::TokenAccounting;

/// Extends [`TokenAccounting`] with security-token-specific storage slots.
///
/// Security identifiers (ISIN, CUSIP, etc.) and redeem parameters are only
/// exposed through the security-token surface, not the base B-20 surface.
pub trait SecurityAccounting: TokenAccounting {
    /// Returns the current share-to-tokens ratio scaled to WAD (1e18).
    fn shares_to_tokens_ratio(&self) -> Result<U256>;
    /// Writes a new share-to-tokens ratio.
    fn set_shares_to_tokens_ratio(&mut self, ratio: U256) -> Result<()>;

    /// Returns the security identifier value for `identifier_type`, or an empty string if unset.
    fn extra_metadata(&self, identifier_type: &str) -> Result<String>;
    /// Writes (or removes when `value` is empty) the security identifier for `identifier_type`.
    fn set_extra_metadata_value(&mut self, identifier_type: &str, value: String) -> Result<()>;

    /// Returns the minimum amount that may be redeemed in a single call.
    fn minimum_redeemable(&self) -> Result<U256>;
    /// Overwrites the minimum redeemable amount.
    fn set_minimum_redeemable(&mut self, minimum: U256) -> Result<()>;

    /// Returns `true` if `id` has been consumed by `announce`.
    fn is_announcement_id_used(&self, id: &str) -> Result<bool>;
    /// Marks `id` as consumed. Called exactly once per announcement id.
    fn mark_announcement_id_used(&mut self, id: &str) -> Result<()>;
}
