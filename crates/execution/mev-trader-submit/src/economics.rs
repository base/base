//! Deterministic ex-ante economics for Blink's pinned-priority backrun.
//!
//! Every monetary value is wei and every per-gas value is wei/gas. The evaluator is
//! deliberately independent of transport: it can only return an inert decision.

use alloy_primitives::U256;

use crate::BLINK_OFA_MIN_KICKBACK_BPS;

const BASIS_POINTS: u64 = 10_000;

/// Same-block authorities that simulation and the OP fee oracle must produce per candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorityEconomicsAuthority {
    execution_gas_estimate: U256,
    l1_data_fee_wei: U256,
    base_fee_per_gas_wei: U256,
    block: u64,
}

impl PriorityEconomicsAuthority {
    /// Captures candidate gas, OP L1 data fee, and Base fee authorities for one block.
    pub const fn new(
        execution_gas_estimate: U256,
        l1_data_fee_wei: U256,
        base_fee_per_gas_wei: U256,
        block: u64,
    ) -> Self {
        Self { execution_gas_estimate, l1_data_fee_wei, base_fee_per_gas_wei, block }
    }

    /// Returns the candidate-specific simulated execution gas, in gas units.
    pub const fn execution_gas_estimate(&self) -> U256 {
        self.execution_gas_estimate
    }

    /// Returns the candidate transaction's OP-stack L1 data fee, in wei.
    pub const fn l1_data_fee_wei(&self) -> U256 {
        self.l1_data_fee_wei
    }

    /// Returns the same-block Base fee authority, in wei/gas.
    pub const fn base_fee_per_gas_wei(&self) -> U256 {
        self.base_fee_per_gas_wei
    }

    /// Returns the block at which all authorities were captured.
    pub const fn block(&self) -> u64 {
        self.block
    }
}

/// Complete, same-frame inputs required by the positive-EV gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PriorityFilterInput {
    /// Gross WETH-denominated output-minus-input, before the on-chain kickback, in wei.
    pub gross_profit_wei: Option<U256>,
    /// Candidate-specific simulation and OP fee-oracle authorities.
    pub authority: Option<PriorityEconomicsAuthority>,
    /// Victim `maxPriorityFeePerGas`, pinned by Blink onto the backrun, in wei/gas.
    pub victim_max_priority_fee_per_gas_wei: Option<U256>,
    /// Victim `maxFeePerGas`, in wei/gas.
    pub victim_max_fee_per_gas_wei: Option<U256>,
    /// Block for which the candidate was quoted.
    pub candidate_block: u64,
}

/// A closed input or arithmetic failure. No variant supplies a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PriorityFilterError {
    /// Gross profit was absent.
    MissingGrossProfit,
    /// Candidate-specific gas and L1 fee authority was absent.
    MissingEconomicsAuthority,
    /// Victim priority fee was absent.
    MissingVictimPriorityFee,
    /// Victim maximum fee was absent.
    MissingVictimMaxFee,
    /// Gross profit was zero.
    ZeroGrossProfit,
    /// Retained value after the kickback was zero.
    ZeroRetainedValue,
    /// Executor gas estimate was zero.
    ZeroGasEstimate,
    /// OP-stack L1 data fee was zero.
    ZeroL1DataFee,
    /// Snapshot base fee was zero.
    ZeroBaseFee,
    /// Victim priority fee was zero.
    ZeroVictimPriorityFee,
    /// Candidate and economics authority came from different blocks.
    StaleEconomicsAuthority,
    /// Victim maximum fee cannot fund the pinned base-plus-priority fee.
    VictimFeeCapTooLow,
    /// Checked value or fee arithmetic overflowed.
    ArithmeticOverflow,
}

/// Checked economics for one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PriorityFilterDecision {
    /// Gross output-minus-input before the executor's mandatory kickback, in wei.
    pub gross_profit_wei: U256,
    /// On-chain `ceil(75% * gross)` kickback, in wei.
    pub kickback_wei: U256,
    /// Gross minus kickback, in wei.
    pub retained_value_wei: U256,
    /// Authoritative candidate-specific execution gas estimate, in gas.
    pub execution_gas_estimate: U256,
    /// L2 execution fee at the pinned effective gas price, in wei.
    pub l2_execution_fee_wei: U256,
    /// OP-stack L1 data fee, in wei.
    pub l1_data_fee_wei: U256,
    /// L2 execution fee plus L1 data fee, in wei.
    pub total_cost_wei: U256,
    /// Positive EV in wei, present only when the candidate is admitted.
    pub expected_ev_wei: Option<U256>,
}

impl PriorityFilterDecision {
    /// Only a strictly positive checked EV is admissible.
    pub(crate) const fn admitted(&self) -> bool {
        self.expected_ev_wei.is_some()
    }
}

fn ceil_kickback(gross: U256) -> Result<U256, PriorityFilterError> {
    let denominator = U256::from(BASIS_POINTS);
    let numerator = U256::from(BLINK_OFA_MIN_KICKBACK_BPS);
    let quotient = gross / denominator;
    let remainder = gross % denominator;
    let whole = quotient.checked_mul(numerator).ok_or(PriorityFilterError::ArithmeticOverflow)?;
    let fractional_product =
        remainder.checked_mul(numerator).ok_or(PriorityFilterError::ArithmeticOverflow)?;
    let fractional = if fractional_product.is_zero() {
        U256::ZERO
    } else {
        ((fractional_product - U256::from(1)) / denominator) + U256::from(1)
    };
    whole.checked_add(fractional).ok_or(PriorityFilterError::ArithmeticOverflow)
}

/// Evaluates Blink pinned-priority economics with checked integer arithmetic.
pub(crate) fn evaluate(
    input: PriorityFilterInput,
) -> Result<PriorityFilterDecision, PriorityFilterError> {
    let gross = input.gross_profit_wei.ok_or(PriorityFilterError::MissingGrossProfit)?;
    let authority = input.authority.ok_or(PriorityFilterError::MissingEconomicsAuthority)?;
    let gas = authority.execution_gas_estimate();
    let l1_data_fee = authority.l1_data_fee_wei();
    let base_fee = authority.base_fee_per_gas_wei();
    let priority = input
        .victim_max_priority_fee_per_gas_wei
        .ok_or(PriorityFilterError::MissingVictimPriorityFee)?;
    let max_fee =
        input.victim_max_fee_per_gas_wei.ok_or(PriorityFilterError::MissingVictimMaxFee)?;

    if gross.is_zero() {
        return Err(PriorityFilterError::ZeroGrossProfit);
    }
    if gas.is_zero() {
        return Err(PriorityFilterError::ZeroGasEstimate);
    }
    if l1_data_fee.is_zero() {
        return Err(PriorityFilterError::ZeroL1DataFee);
    }
    if base_fee.is_zero() {
        return Err(PriorityFilterError::ZeroBaseFee);
    }
    if priority.is_zero() {
        return Err(PriorityFilterError::ZeroVictimPriorityFee);
    }
    if input.candidate_block != authority.block() {
        return Err(PriorityFilterError::StaleEconomicsAuthority);
    }

    let kickback = ceil_kickback(gross)?;
    let retained = gross.checked_sub(kickback).ok_or(PriorityFilterError::ArithmeticOverflow)?;
    if retained.is_zero() {
        return Err(PriorityFilterError::ZeroRetainedValue);
    }
    let effective_fee =
        base_fee.checked_add(priority).ok_or(PriorityFilterError::ArithmeticOverflow)?;
    if max_fee < effective_fee {
        return Err(PriorityFilterError::VictimFeeCapTooLow);
    }
    let l2_execution_fee =
        gas.checked_mul(effective_fee).ok_or(PriorityFilterError::ArithmeticOverflow)?;
    let total_cost =
        l2_execution_fee.checked_add(l1_data_fee).ok_or(PriorityFilterError::ArithmeticOverflow)?;
    let expected_ev = retained.checked_sub(total_cost).filter(|ev| !ev.is_zero());

    Ok(PriorityFilterDecision {
        gross_profit_wei: gross,
        kickback_wei: kickback,
        retained_value_wei: retained,
        execution_gas_estimate: gas,
        l2_execution_fee_wei: l2_execution_fee,
        l1_data_fee_wei: l1_data_fee,
        total_cost_wei: total_cost,
        expected_ev_wei: expected_ev,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(gross: u64, gas: u64, base: u64, priority: u64, l1_fee: u64) -> PriorityFilterInput {
        PriorityFilterInput {
            gross_profit_wei: Some(U256::from(gross)),
            authority: Some(PriorityEconomicsAuthority::new(
                U256::from(gas),
                U256::from(l1_fee),
                U256::from(base),
                42,
            )),
            victim_max_priority_fee_per_gas_wei: Some(U256::from(priority)),
            victim_max_fee_per_gas_wei: Some(U256::from(base + priority)),
            candidate_block: 42,
        }
    }

    #[test]
    fn strict_threshold_includes_ceil_kickback_l2_and_l1_fees() {
        // L2 = 100 and L1 = 10, so retained value must be strictly above 110.
        let below = evaluate(input(439, 10, 5, 5, 10)).unwrap();
        let equal = evaluate(input(443, 10, 5, 5, 10)).unwrap();
        let above = evaluate(input(445, 10, 5, 5, 10)).unwrap();
        assert!(!below.admitted());
        assert!(!equal.admitted());
        assert_eq!(equal.retained_value_wei, U256::from(110));
        assert_eq!(equal.total_cost_wei, U256::from(110));
        assert_eq!(above.expected_ev_wei, Some(U256::from(1)));
    }

    #[test]
    fn kickback_rounding_matches_floor_quarter_retained_for_edge_values() {
        for gross in 1_u64..=20_001 {
            let kickback = ceil_kickback(U256::from(gross)).unwrap();
            let retained = U256::from(gross) - kickback;
            assert_eq!(retained, U256::from(gross / 4));
        }
    }

    #[test]
    fn exact_relationship_uses_every_economics_term() {
        let decision = evaluate(input(1_000_000, 2_000, 10, 10, 7_000)).unwrap();
        assert_eq!(decision.kickback_wei, U256::from(750_000));
        assert_eq!(decision.retained_value_wei, U256::from(250_000));
        assert_eq!(decision.execution_gas_estimate, U256::from(2_000));
        assert_eq!(decision.l2_execution_fee_wei, U256::from(40_000));
        assert_eq!(decision.l1_data_fee_wei, U256::from(7_000));
        assert_eq!(decision.total_cost_wei, U256::from(47_000));
        assert_eq!(
            decision.expected_ev_wei,
            decision.retained_value_wei.checked_sub(
                decision
                    .execution_gas_estimate
                    .checked_mul(U256::from(20))
                    .unwrap()
                    .checked_add(decision.l1_data_fee_wei)
                    .unwrap(),
            )
        );
    }

    #[test]
    fn authoritative_gas_and_l1_fee_change_admission() {
        assert!(evaluate(input(445, 10, 5, 5, 10)).unwrap().admitted());
        assert!(!evaluate(input(445, 11, 5, 5, 10)).unwrap().admitted());
        assert!(!evaluate(input(445, 10, 5, 5, 12)).unwrap().admitted());
    }

    #[test]
    fn missing_zero_and_stale_authorities_fail_closed() {
        let mut case = input(1_000, 10, 5, 5, 10);
        case.authority = None;
        assert_eq!(evaluate(case), Err(PriorityFilterError::MissingEconomicsAuthority));

        let mut case = input(1_000, 0, 5, 5, 10);
        assert_eq!(evaluate(case), Err(PriorityFilterError::ZeroGasEstimate));

        case = input(1_000, 10, 5, 5, 0);
        assert_eq!(evaluate(case), Err(PriorityFilterError::ZeroL1DataFee));

        case = input(1_000, 10, 0, 5, 10);
        assert_eq!(evaluate(case), Err(PriorityFilterError::ZeroBaseFee));

        case = input(3, 10, 5, 5, 10);
        assert_eq!(evaluate(case), Err(PriorityFilterError::ZeroRetainedValue));

        case = input(1_000, 10, 5, 5, 10);
        case.candidate_block += 1;
        assert_eq!(evaluate(case), Err(PriorityFilterError::StaleEconomicsAuthority));
    }

    #[test]
    fn arithmetic_overflow_fails_closed() {
        let mut case = input(1_000, 10, 5, 5, 10);
        case.authority =
            Some(PriorityEconomicsAuthority::new(U256::from(10), U256::from(1), U256::MAX, 42));
        assert_eq!(evaluate(case), Err(PriorityFilterError::ArithmeticOverflow));

        case = input(1_000, 10, 5, 5, 10);
        case.authority =
            Some(PriorityEconomicsAuthority::new(U256::MAX, U256::from(1), U256::from(5), 42));
        assert_eq!(evaluate(case), Err(PriorityFilterError::ArithmeticOverflow));

        case = input(1_000, 1, 1, 1, 10);
        case.authority =
            Some(PriorityEconomicsAuthority::new(U256::from(1), U256::MAX, U256::from(1), 42));
        assert_eq!(evaluate(case), Err(PriorityFilterError::ArithmeticOverflow));
    }
}
