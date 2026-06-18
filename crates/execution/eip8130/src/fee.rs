//! EIP-8130 fee-cap and gas-payer balance validation.

use alloy_primitives::U256;

/// Reason a transaction's fees or the payer's balance are invalid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FeeError {
    /// `max_priority_fee_per_gas` exceeds `max_fee_per_gas`.
    #[error("max_priority_fee_per_gas {tip} exceeds max_fee_per_gas {max_fee}")]
    TipAboveFeeCap {
        /// The transaction's priority fee cap.
        tip: u128,
        /// The transaction's total fee cap.
        max_fee: u128,
    },

    /// `max_fee_per_gas` is below the block base fee, so the transaction cannot
    /// be included at the current base fee.
    #[error("max_fee_per_gas {max_fee} is below the base fee {base_fee}")]
    FeeCapBelowBaseFee {
        /// The block's base fee per gas.
        base_fee: u128,
        /// The transaction's total fee cap.
        max_fee: u128,
    },

    /// The gas payer's balance cannot cover the worst-case gas charge.
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
    /// EIP-1559 effective gas price: `min(max_fee, base_fee + tip)`.
    #[must_use]
    pub const fn effective_gas_price(
        max_fee: u128,
        max_priority_fee: u128,
        base_fee: u128,
    ) -> u128 {
        // `<u128 as Ord>::min` is not yet const-stable, so take the min by hand
        // to keep this `const fn` (consistent with the other `FeeCheck` helpers).
        let cap = base_fee.saturating_add(max_priority_fee);
        if max_fee < cap { max_fee } else { cap }
    }

    /// Maximum gas the payer can be charged: `gas_limit + payer_auth_cost`. Payer
    /// authentication is metered on top of `gas_limit`, so the payer reimburses
    /// it in addition to the sender-signed budget. For self-pay
    /// (`payer_auth_cost == 0`) this is just `gas_limit`.
    #[must_use]
    pub const fn max_chargeable_gas(gas_limit: u64, payer_auth_cost: u64) -> u64 {
        gas_limit.saturating_add(payer_auth_cost)
    }

    /// Upper bound on the payer's ETH debit:
    /// `(gas_limit + payer_auth_cost) · max_fee_per_gas`.
    #[must_use]
    pub fn max_fee_charge(gas_limit: u64, payer_auth_cost: u64, max_fee: u128) -> U256 {
        U256::from(Self::max_chargeable_gas(gas_limit, payer_auth_cost))
            .saturating_mul(U256::from(max_fee))
    }

    /// Validates the EIP-1559 fee caps against the block base fee.
    ///
    /// # Errors
    /// - [`FeeError::TipAboveFeeCap`] — the priority fee cap exceeds the total cap.
    /// - [`FeeError::FeeCapBelowBaseFee`] — the total cap is below the base fee.
    #[must_use = "discarding the result silently skips the fee-cap check"]
    pub const fn validate_fees(
        max_fee: u128,
        max_priority_fee: u128,
        base_fee: u128,
    ) -> Result<(), FeeError> {
        if max_priority_fee > max_fee {
            return Err(FeeError::TipAboveFeeCap { tip: max_priority_fee, max_fee });
        }
        if max_fee < base_fee {
            return Err(FeeError::FeeCapBelowBaseFee { base_fee, max_fee });
        }
        Ok(())
    }

    /// Validates that the gas payer (a sponsor, or the sender for self-pay) can
    /// cover the worst-case gas charge at `max_fee_per_gas`.
    ///
    /// # Errors
    /// - [`FeeError::InsufficientBalance`] — the balance is below the maximum charge.
    #[must_use = "discarding the result silently skips the balance check"]
    pub fn validate_balance(
        balance: U256,
        gas_limit: u64,
        payer_auth_cost: u64,
        max_fee: u128,
    ) -> Result<(), FeeError> {
        let required = Self::max_fee_charge(gas_limit, payer_auth_cost, max_fee);
        if balance < required {
            return Err(FeeError::InsufficientBalance { balance, required });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_gas_price_is_capped_by_fee_cap() {
        // base + tip below the cap -> base + tip.
        assert_eq!(FeeCheck::effective_gas_price(100, 5, 80), 85);
        // base + tip above the cap -> capped at max_fee.
        assert_eq!(FeeCheck::effective_gas_price(100, 50, 80), 100);
    }

    #[test]
    fn validate_fees_rules() {
        assert_eq!(FeeCheck::validate_fees(100, 10, 50), Ok(()));
        assert_eq!(
            FeeCheck::validate_fees(100, 150, 50),
            Err(FeeError::TipAboveFeeCap { tip: 150, max_fee: 100 })
        );
        assert_eq!(
            FeeCheck::validate_fees(40, 10, 50),
            Err(FeeError::FeeCapBelowBaseFee { base_fee: 50, max_fee: 40 })
        );
    }

    #[test]
    fn max_fee_charge_includes_payer_auth_on_top_of_gas_limit() {
        // self-pay: gas_limit * max_fee.
        assert_eq!(FeeCheck::max_fee_charge(21_000, 0, 2), U256::from(42_000u64));
        // sponsored: (gas_limit + payer_auth_cost) * max_fee.
        assert_eq!(FeeCheck::max_fee_charge(21_000, 5_100, 2), U256::from(52_200u64));
    }

    #[test]
    fn validate_balance_rules() {
        // required = 21_000 * 2 = 42_000.
        assert_eq!(FeeCheck::validate_balance(U256::from(42_000u64), 21_000, 0, 2), Ok(()));
        assert_eq!(
            FeeCheck::validate_balance(U256::from(41_999u64), 21_000, 0, 2),
            Err(FeeError::InsufficientBalance {
                balance: U256::from(41_999u64),
                required: U256::from(42_000u64),
            })
        );
    }
}
