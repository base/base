//! Pause-bit helpers for B-20 tokens.

use alloy_primitives::U256;

use crate::IB20;

/// Helpers for mapping B-20 pausable features into storage bits.
#[derive(Debug, Clone, Copy)]
pub struct B20PausableFeature;

impl B20PausableFeature {
    /// Returns the storage bit for a pausable feature.
    pub fn mask(feature: IB20::PausableFeature) -> U256 {
        U256::ONE.checked_shl(usize::from(feature as u8)).unwrap_or(U256::ZERO)
    }
}
