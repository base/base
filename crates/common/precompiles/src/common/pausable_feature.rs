//! Pause-bit helpers for B-20 tokens.

use alloy_primitives::U256;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::IB20;

/// Helpers for mapping B-20 pausable features into storage bits.
#[derive(Debug, Clone, Copy)]
pub struct B20PausableFeature;

impl B20PausableFeature {
    /// Returns an enum-conversion panic when `feature` is outside the B-20 pause enum.
    pub const fn ensure_valid(feature: IB20::PausableFeature) -> Result<()> {
        match feature {
            IB20::PausableFeature::TRANSFER
            | IB20::PausableFeature::MINT
            | IB20::PausableFeature::BURN => Ok(()),
            IB20::PausableFeature::__Invalid => Err(BasePrecompileError::enum_conversion_error()),
        }
    }

    /// Returns the storage bit for a pausable feature.
    pub fn mask(feature: IB20::PausableFeature) -> U256 {
        U256::ONE.checked_shl(usize::from(feature as u8)).unwrap_or(U256::ZERO)
    }
}
