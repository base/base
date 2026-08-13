//! Pause-bit helpers for B-20 tokens.

use alloy_primitives::U256;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::IB20;

/// Helpers for mapping B-20 pausable features into storage bits.
#[derive(Debug, Clone, Copy)]
pub struct B20PausableFeature;

impl B20PausableFeature {
    /// Returns an enum-conversion error when `feature` is not in `allowed`.
    ///
    /// Each frozen version passes its own allowlist so a feature introduced at a later fork is
    /// rejected by omission on older versions (e.g. V1 allows only TRANSFER/MINT/BURN; V2 adds
    /// SEIZE). New versions extend their local list; already-shipped versions stay untouched.
    pub fn ensure_one_of(
        feature: IB20::PausableFeature,
        allowed: &[IB20::PausableFeature],
    ) -> Result<()> {
        if allowed.contains(&feature) {
            Ok(())
        } else {
            Err(BasePrecompileError::enum_conversion_error())
        }
    }

    /// Returns the storage bit for a pausable feature.
    pub fn mask(feature: IB20::PausableFeature) -> U256 {
        U256::ONE.checked_shl(usize::from(feature as u8)).unwrap_or(U256::ZERO)
    }
}
