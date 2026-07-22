//! Token/native exchange rate and ERC-8168 token-payment math.

use alloy_primitives::U256;

use crate::error::PricingError;

/// A token-per-native exchange rate as an exact rational: [`numerator`] token
/// atomic units per [`denominator`] native wei. Mirrors the ERC-8168
/// `TokenChoice.rate` `{ numerator, denominator }` shape, so a payer can quote
/// it verbatim and a wallet (or the builder at co-sign) can re-derive the
/// phase-0 `paymentAmount` from cached terms.
///
/// [`numerator`]: Self::numerator
/// [`denominator`]: Self::denominator
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    /// Token atomic units.
    pub numerator: U256,
    /// Native wei that [`Self::numerator`] token atomic units are worth.
    pub denominator: U256,
}

impl Rate {
    /// Constructs a rate from its raw `numerator` / `denominator`.
    pub const fn new(numerator: U256, denominator: U256) -> Self {
        Self { numerator, denominator }
    }

    /// The gross phase-0 token transfer that covers `gas_limit ×
    /// max_fee_per_gas` wei of gas at this rate, inflated by `margin_bps` and
    /// rounded **up** so the payer is never short.
    ///
    /// This is ERC-8168's `ceil(gas_limit × max_fee_per_gas × rate.numerator /
    /// rate.denominator)` with the payer margin folded in as a `(10_000 +
    /// margin_bps) / 10_000` multiplier. All intermediate products are checked,
    /// returning [`PricingError::Overflow`] rather than wrapping.
    pub fn payment_amount(
        &self,
        gas_limit: u64,
        max_fee_per_gas: u128,
        margin_bps: u16,
    ) -> Result<U256, PricingError> {
        if self.denominator.is_zero() {
            return Err(PricingError::ZeroDenominator);
        }

        let cost = U256::from(gas_limit)
            .checked_mul(U256::from(max_fee_per_gas))
            .ok_or(PricingError::Overflow)?;

        // Fold the margin into the rational: numerator × (10_000 + margin_bps)
        // over denominator × 10_000.
        let margin_num = U256::from(10_000u32 + u32::from(margin_bps));
        let num = self.numerator.checked_mul(margin_num).ok_or(PricingError::Overflow)?;
        let den = self.denominator.checked_mul(U256::from(10_000u32)).ok_or(PricingError::Overflow)?;

        let scaled = cost.checked_mul(num).ok_or(PricingError::Overflow)?;
        Self::ceil_div(scaled, den)
    }

    /// `ceil(a / b)` for `U256`, guarding the `+ (b - 1)` against overflow.
    fn ceil_div(a: U256, b: U256) -> Result<U256, PricingError> {
        if b.is_zero() {
            return Err(PricingError::ZeroDenominator);
        }
        let bumped = a.checked_add(b - U256::from(1u8)).ok_or(PricingError::Overflow)?;
        Ok(bumped / b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_amount_exact_no_margin() {
        // rate = 1 token-atomic per 4e8 wei; cost = 1e18 wei → 2.5e9 atomic.
        let rate = Rate::new(U256::from(1u64), U256::from(400_000_000u64));
        // 1e9 gas × 1e9 wei/gas = 1e18 wei.
        let amount = rate.payment_amount(1_000_000_000, 1_000_000_000, 0).unwrap();
        assert_eq!(amount, U256::from(2_500_000_000u64));
    }

    #[test]
    fn payment_amount_applies_margin() {
        let rate = Rate::new(U256::from(1u64), U256::from(400_000_000u64));
        // +1% margin over 2.5e9 → 2.525e9.
        let amount = rate.payment_amount(1_000_000_000, 1_000_000_000, 100).unwrap();
        assert_eq!(amount, U256::from(2_525_000_000u64));
    }

    #[test]
    fn payment_amount_rounds_up() {
        // cost = 1 wei, rate = 1/3 atomic per wei → ceil(1/3) = 1.
        let rate = Rate::new(U256::from(1u64), U256::from(3u64));
        let amount = rate.payment_amount(1, 1, 0).unwrap();
        assert_eq!(amount, U256::from(1u64));
    }

    #[test]
    fn zero_denominator_is_rejected() {
        let rate = Rate::new(U256::from(1u64), U256::ZERO);
        assert_eq!(rate.payment_amount(1, 1, 0), Err(PricingError::ZeroDenominator));
    }

    #[test]
    fn zero_cost_is_zero_amount() {
        let rate = Rate::new(U256::from(1u64), U256::from(3u64));
        assert_eq!(rate.payment_amount(0, 1, 500).unwrap(), U256::ZERO);
    }
}
