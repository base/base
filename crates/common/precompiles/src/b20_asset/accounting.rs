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

    /// Returns the pending scheduled multiplier target (ERC-8056)
    ///
    /// Packs with [`Self::pending_effective_at`] into a single slot. Introduced with the
    /// scheduled-multiplier feature (`AssetV2`); earlier versions never touch this slot.
    fn pending_multiplier(&self) -> Result<u128> {
        reject_frozen_selector!()
    }
    /// Returns the timestamp at which the pending multiplier becomes effective; `0` means none.
    fn pending_effective_at(&self) -> Result<u64> {
        reject_frozen_selector!()
    }
    /// Writes the pending schedule.
    fn set_pending_and_effective_at(
        &mut self,
        _multiplier: u128,
        _effective_at: u64,
    ) -> Result<()> {
        reject_frozen_selector!()
    }
    /// Clears the pending schedule, restoring the no-pending state.
    fn clear_pending_multiplier_and_effective_at(&mut self) -> Result<()> {
        reject_frozen_selector!()
    }

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
