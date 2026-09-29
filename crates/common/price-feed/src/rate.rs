//! ERC-8168 token rates and phase-0 payment amounts.

use alloy_primitives::{U256, U512};

/// Token atomic units per 10^18 wei (one ETH): the ERC-8168 `rate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TokenRate(pub U256);

impl TokenRate {
    /// Wei per ETH, the unit a rate is quoted against.
    pub const WEI_PER_ETH: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

    /// Basis points in 100%.
    pub const BASIS_POINTS: u64 = 10_000;

    /// Returns this rate raised by `spread_bps` basis points, rounded up so the
    /// spread is never understated. Returns `None` on overflow.
    pub fn with_spread(self, spread_bps: u32) -> Option<Self> {
        let scaled = U512::from(self.0) * U512::from(Self::BASIS_POINTS + u64::from(spread_bps));
        let rate = scaled.div_ceil(U512::from(Self::BASIS_POINTS));
        U256::checked_from_limbs_slice(rate.as_limbs()).map(Self)
    }

    /// Returns the phase-0 payment required for a transaction with `gas_limit`
    /// and `max_fee_per_gas`: `ceil(gas_limit * max_fee_per_gas * rate / 10^18)`,
    /// computed exactly as ERC-8168 specifies. Returns `None` on overflow.
    pub fn required_amount(self, gas_limit: u64, max_fee_per_gas: u128) -> Option<U256> {
        let max_cost = U512::from(gas_limit) * U512::from(max_fee_per_gas);
        let amount = (max_cost * U512::from(self.0)).div_ceil(U512::from(Self::WEI_PER_ETH));
        U256::checked_from_limbs_slice(amount.as_limbs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2,000 USDC per ETH.
    const USDC_RATE: TokenRate = TokenRate(U256::from_limbs([0x7735_9400, 0, 0, 0]));

    #[test]
    fn matches_erc_8168_payment_examples() {
        // 70,000 gas at 1.5 gwei.
        assert_eq!(USDC_RATE.required_amount(0x11170, 0x5968_2F00), Some(U256::from(0x33450u64)));
        assert_eq!(
            TokenRate(U256::from(0x9502_F900u64)).required_amount(0x11170, 0x5968_2F00),
            Some(U256::from(0x40164u64))
        );
        // 70,000 gas at 3.5 gwei.
        assert_eq!(USDC_RATE.required_amount(0x11170, 0xD09D_C300), Some(U256::from(0x77A10u64)));
    }

    #[test]
    fn rounds_required_amount_up() {
        assert_eq!(TokenRate(U256::from(1)).required_amount(1, 1), Some(U256::from(1)));
        assert_eq!(TokenRate(U256::from(1)).required_amount(0, 1), Some(U256::ZERO));
    }

    #[test]
    fn required_amount_overflow_is_none() {
        assert_eq!(TokenRate(U256::MAX).required_amount(u64::MAX, u128::MAX), None);
    }

    #[test]
    fn applies_spread_rounding_up() {
        assert_eq!(USDC_RATE.with_spread(60), Some(TokenRate(U256::from(2_012_000_000u64))));
        assert_eq!(TokenRate(U256::from(1)).with_spread(1), Some(TokenRate(U256::from(2))));
        assert_eq!(USDC_RATE.with_spread(0), Some(USDC_RATE));
        assert_eq!(TokenRate(U256::MAX).with_spread(1), None);
    }
}
