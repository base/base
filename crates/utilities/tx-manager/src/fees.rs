//! Fee calculation and bumping logic.

use alloy_primitives::U256;

/// Calculates and bumps transaction fees.
#[derive(Debug)]
pub struct FeeCalculator;

/// Intermediate fee estimates computed during gas price suggestion.
///
/// Used between fee calculation and transaction construction to carry
/// the tip cap, base fee cap, and optional blob fee cap.
#[derive(Debug, Clone, Default)]
pub struct GasPriceCaps {
    /// Maximum priority fee per gas (tip).
    pub gas_tip_cap: U256,
    /// Maximum total fee per gas (base fee + tip).
    pub gas_fee_cap: U256,
    /// Maximum blob fee per gas (for EIP-4844 txs). `None` for non-blob txs.
    pub blob_fee_cap: Option<U256>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_zeroes_all_fields() {
        let caps = GasPriceCaps::default();

        assert_eq!(caps.gas_tip_cap, U256::ZERO);
        assert_eq!(caps.gas_fee_cap, U256::ZERO);
        assert!(caps.blob_fee_cap.is_none());
    }

    #[test]
    fn gas_price_caps_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GasPriceCaps>();
    }
}
