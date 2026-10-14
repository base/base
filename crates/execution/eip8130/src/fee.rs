//! EIP-8130 fee-budget and gas-payer balance validation.
//!
//! An EIP-8130 transaction carries one total `max_fee` in wei, as in
//! [EIP-7999], instead of a per-gas cap. The budget covers every protocol fee
//! the payer is charged: L2 gas at the base fee, the priority fee, the L1 data
//! fee, and the operator fee.
//!
//! [EIP-7999]: https://eips.ethereum.org/EIPS/eip-7999

use alloy_primitives::U256;

/// Reason a transaction's fees or the payer's balance are invalid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FeeError {
    /// `max_fee` cannot cover the worst-case cost at the block base fee: the
    /// full gas reservation at the base fee plus the L1 and operator fees.
    #[error("max_fee {max_fee} is below the required {required}")]
    BudgetBelowRequired {
        /// The cost the budget must cover.
        required: U256,
        /// The transaction's total fee budget.
        max_fee: u128,
    },

    /// The gas payer's balance cannot cover the worst-case fee charge.
    #[error("gas payer balance {balance} is below the required {required}")]
    InsufficientBalance {
        /// The payer's available balance.
        balance: U256,
        /// The maximum charge the payer must be able to cover.
        required: U256,
    },
}

/// Stateless EIP-8130 fee and balance checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FeeCheck;

impl FeeCheck {
    /// Maximum gas the payer can be charged: `gas_limit + payer_auth_cost`. Payer
    /// authentication is metered on top of `gas_limit`, so the payer reimburses
    /// it in addition to the sender-signed budget. For self-pay
    /// (`payer_auth_cost == 0`) this is just `gas_limit`.
    #[must_use]
    pub const fn max_chargeable_gas(gas_limit: u64, payer_auth_cost: u64) -> u64 {
        gas_limit.saturating_add(payer_auth_cost)
    }

    /// Worst-case cost at `gas_price`: `max_gas · gas_price + additional_fees`,
    /// where `additional_fees` is the L1 data fee plus the operator fee charged
    /// on `max_gas`.
    #[must_use]
    pub fn max_fee_charge(max_gas: u64, gas_price: u128, additional_fees: U256) -> U256 {
        U256::from(max_gas).saturating_mul(U256::from(gas_price)).saturating_add(additional_fees)
    }

    /// Validates that `max_fee` covers the worst-case cost at the base fee and
    /// returns the effective gas price.
    ///
    /// The priority fee per gas is the transaction's tip, capped so the full
    /// reservation still fits the budget:
    ///
    /// ```text
    /// required  = max_gas · base_fee + additional_fees
    /// effective = base_fee + min(max_priority_fee, (max_fee − required) / max_gas)
    /// ```
    ///
    /// so [`Self::max_fee_charge`] at the returned price never exceeds
    /// `max_fee`. EIP-7999 caps the priority fee as a wei total; flooring it to a
    /// per-gas price forgoes less than `max_gas` wei of tip and keeps a single
    /// per-gas price for receipts, `GASPRICE`, and settlement.
    ///
    /// # Errors
    /// - [`FeeError::BudgetBelowRequired`] — `max_fee` is below `required`.
    pub fn effective_gas_price(
        max_fee: u128,
        max_priority_fee: u128,
        base_fee: u128,
        max_gas: u64,
        additional_fees: U256,
    ) -> Result<u128, FeeError> {
        let required = Self::max_fee_charge(max_gas, base_fee, additional_fees);
        let headroom = U256::from(max_fee)
            .checked_sub(required)
            .ok_or(FeeError::BudgetBelowRequired { required, max_fee })?;
        // `headroom <= max_fee`, so the per-gas share fits in `u128`.
        let tip_room = headroom.checked_div(U256::from(max_gas)).unwrap_or_default().to::<u128>();
        Ok(base_fee.saturating_add(max_priority_fee.min(tip_room)))
    }

    /// Validates that the gas payer (a sponsor, or the sender for self-pay) can
    /// cover `required`, the worst-case fee charge.
    ///
    /// # Errors
    /// - [`FeeError::InsufficientBalance`] — the balance is below `required`.
    #[must_use = "discarding the result silently skips the balance check"]
    pub fn validate_balance(balance: U256, required: U256) -> Result<(), FeeError> {
        if balance < required {
            return Err(FeeError::InsufficientBalance { balance, required });
        }
        Ok(())
    }

    /// Validates that the full fee budget and a declared coinbase tip are
    /// payable.
    ///
    /// The payer can be charged at most `max_fee`. Self-pay
    /// (`payer_is_sender`) requires one balance to cover `max_fee + tip`.
    /// Sponsored payment requires the payer to cover `max_fee` and the sender to
    /// cover the tip.
    ///
    /// # Errors
    /// - [`FeeError::InsufficientBalance`] — a required balance is below its charge.
    #[must_use = "discarding the result silently skips the fee-and-tip check"]
    pub fn validate_fee_and_tip(
        payer_balance: U256,
        sender_balance: U256,
        payer_is_sender: bool,
        max_fee: u128,
        tip: U256,
    ) -> Result<(), FeeError> {
        let fee = U256::from(max_fee);
        if payer_is_sender {
            return Self::validate_balance(payer_balance, fee.saturating_add(tip));
        }
        Self::validate_balance(payer_balance, fee)?;
        Self::validate_balance(sender_balance, tip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tip_is_paid_in_full_when_the_budget_has_room() {
        // required = 100 * 10 + 500 = 1_500; tip room = (10_000 - 1_500) / 100 = 85.
        assert_eq!(FeeCheck::effective_gas_price(10_000, 5, 10, 100, U256::from(500u64)), Ok(15));
    }

    #[test]
    fn tip_is_capped_by_the_remaining_budget() {
        // required = 1_500; tip room = (2_000 - 1_500) / 100 = 5.
        assert_eq!(FeeCheck::effective_gas_price(2_000, 50, 10, 100, U256::from(500u64)), Ok(15));
        // Exactly the required amount leaves no tip.
        assert_eq!(FeeCheck::effective_gas_price(1_500, 50, 10, 100, U256::from(500u64)), Ok(10));
    }

    #[test]
    fn budget_below_base_cost_is_rejected() {
        // The L1 and operator fees count against the budget.
        assert_eq!(
            FeeCheck::effective_gas_price(1_499, 0, 10, 100, U256::from(500u64)),
            Err(FeeError::BudgetBelowRequired { required: U256::from(1_500u64), max_fee: 1_499 })
        );
    }

    #[test]
    fn worst_case_charge_at_effective_price_fits_the_budget() {
        for (max_fee, tip, base_fee, max_gas, additional) in [
            (10_000u128, 5u128, 10u128, 100u64, 500u64),
            (2_001, 50, 10, 100, 500),
            (1_000_003, u128::MAX, 7, 333, 12_345),
            (u128::MAX, u128::MAX, 1, 30_000_000, 1),
        ] {
            let additional = U256::from(additional);
            let effective =
                FeeCheck::effective_gas_price(max_fee, tip, base_fee, max_gas, additional).unwrap();
            assert!(
                FeeCheck::max_fee_charge(max_gas, effective, additional) <= U256::from(max_fee),
                "max_fee={max_fee} effective={effective}"
            );
        }
    }

    #[test]
    fn max_fee_charge_adds_l1_and_operator_fees_to_gas() {
        assert_eq!(FeeCheck::max_fee_charge(21_000, 2, U256::from(7u64)), U256::from(42_007u64));
    }

    #[test]
    fn validate_balance_rules() {
        assert_eq!(FeeCheck::validate_balance(U256::from(42u64), U256::from(42u64)), Ok(()));
        assert_eq!(
            FeeCheck::validate_balance(U256::from(41u64), U256::from(42u64)),
            Err(FeeError::InsufficientBalance {
                balance: U256::from(41u64),
                required: U256::from(42u64),
            })
        );
    }

    #[test]
    fn self_pay_requires_budget_plus_tip_from_one_balance() {
        let tip = U256::from(1_000u64);
        let both = |balance: u64| U256::from(balance);
        assert_eq!(
            FeeCheck::validate_fee_and_tip(both(43_000), both(43_000), true, 42_000, tip),
            Ok(())
        );
        assert_eq!(
            FeeCheck::validate_fee_and_tip(both(42_999), both(42_999), true, 42_000, tip),
            Err(FeeError::InsufficientBalance {
                balance: U256::from(42_999u64),
                required: U256::from(43_000u64),
            })
        );
    }

    #[test]
    fn sponsored_checks_payer_budget_and_sender_tip_separately() {
        let tip = U256::from(1_000u64);
        let payer = U256::from(42_000u64);
        let sender = U256::from(1_000u64);
        assert_eq!(FeeCheck::validate_fee_and_tip(payer, sender, false, 42_000, tip), Ok(()));
        assert_eq!(
            FeeCheck::validate_fee_and_tip(U256::from(41_999u64), sender, false, 42_000, tip),
            Err(FeeError::InsufficientBalance { balance: U256::from(41_999u64), required: payer })
        );
        assert_eq!(
            FeeCheck::validate_fee_and_tip(payer, U256::from(999u64), false, 42_000, tip),
            Err(FeeError::InsufficientBalance { balance: U256::from(999u64), required: tip })
        );
    }
}
