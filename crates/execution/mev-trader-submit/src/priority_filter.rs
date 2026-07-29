//! Deterministic ex-ante economics for Blink's pinned-priority backrun.
//!
//! Every monetary value is wei and every per-gas value is wei/gas. The evaluator is
//! deliberately independent of transport: it can only return an inert decision.

use alloy_primitives::U256;
const PARTS_PER_MILLION: u64 = 1_000_000;

/// Complete, same-frame inputs required by the positive-EV gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PriorityFilterInput {
    /// Gross WETH-denominated candidate value, in wei.
    pub candidate_value_wei: Option<U256>,
    /// Executor gas estimate, in gas units.
    pub gas_estimate: Option<U256>,
    /// Snapshot base fee, in wei/gas.
    pub base_fee_per_gas_wei: Option<U256>,
    /// Victim `maxPriorityFeePerGas`, pinned by Blink onto the backrun, in wei/gas.
    pub victim_max_priority_fee_per_gas_wei: Option<U256>,
    /// Victim `maxFeePerGas`, in wei/gas.
    pub victim_max_fee_per_gas_wei: Option<U256>,
    /// Block for which the candidate was quoted.
    pub candidate_block: u64,
    /// Block from which the fee authority was captured.
    pub fee_block: u64,
}

/// A closed input or arithmetic failure. No variant supplies a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PriorityFilterError {
    /// Candidate value was absent.
    MissingCandidateValue,
    /// Executor gas estimate was absent.
    MissingGasEstimate,
    /// Snapshot base fee was absent.
    MissingBaseFee,
    /// Victim priority fee was absent.
    MissingVictimPriorityFee,
    /// Victim maximum fee was absent.
    MissingVictimMaxFee,
    /// Candidate value was zero.
    ZeroCandidateValue,
    /// Executor gas estimate was zero.
    ZeroGasEstimate,
    /// Snapshot base fee was zero.
    ZeroBaseFee,
    /// Victim priority fee was zero.
    ZeroVictimPriorityFee,
    /// Candidate and fee authority came from different blocks.
    StaleFeeAuthority,
    /// Victim maximum fee cannot fund the pinned base-plus-priority fee.
    VictimFeeCapTooLow,
    /// Checked fee or gas arithmetic overflowed.
    ArithmeticOverflow,
}

/// Checked economics for one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PriorityFilterDecision {
    /// `gas_estimate * (base_fee + pinned victim priority)`, in wei.
    pub expected_cost_wei: U256,
    /// Positive EV in wei, present only when the candidate is admitted.
    pub expected_ev_wei: Option<U256>,
    /// Floor of `(value - base-fee cost) / gas`, in wei/gas.
    pub break_even_priority_fee_per_gas_wei_floor: U256,
    /// Remainder of `(value - base-fee cost) / gas`, in wei.
    pub break_even_remainder_wei: U256,
    /// Break-even priority divided by base fee, in parts per million.
    pub break_even_priority_to_base_fee_ppm: U256,
    /// Greatest integral priority fee that still leaves at least one wei of EV.
    pub max_profitable_priority_fee_per_gas_wei: Option<U256>,
}

impl PriorityFilterDecision {
    /// Only a strictly positive checked EV is admissible.
    pub(crate) const fn admitted(&self) -> bool {
        self.expected_ev_wei.is_some()
    }
}

/// Evaluates Blink pinned-priority economics with checked integer arithmetic.
pub(crate) fn evaluate(
    input: PriorityFilterInput,
) -> Result<PriorityFilterDecision, PriorityFilterError> {
    let value = input.candidate_value_wei.ok_or(PriorityFilterError::MissingCandidateValue)?;
    let gas = input.gas_estimate.ok_or(PriorityFilterError::MissingGasEstimate)?;
    let base_fee = input.base_fee_per_gas_wei.ok_or(PriorityFilterError::MissingBaseFee)?;
    let priority = input
        .victim_max_priority_fee_per_gas_wei
        .ok_or(PriorityFilterError::MissingVictimPriorityFee)?;
    let max_fee =
        input.victim_max_fee_per_gas_wei.ok_or(PriorityFilterError::MissingVictimMaxFee)?;

    if value.is_zero() {
        return Err(PriorityFilterError::ZeroCandidateValue);
    }
    if gas.is_zero() {
        return Err(PriorityFilterError::ZeroGasEstimate);
    }
    if base_fee.is_zero() {
        return Err(PriorityFilterError::ZeroBaseFee);
    }
    if priority.is_zero() {
        return Err(PriorityFilterError::ZeroVictimPriorityFee);
    }
    if input.candidate_block != input.fee_block {
        return Err(PriorityFilterError::StaleFeeAuthority);
    }

    let effective_fee =
        base_fee.checked_add(priority).ok_or(PriorityFilterError::ArithmeticOverflow)?;
    if max_fee < effective_fee {
        return Err(PriorityFilterError::VictimFeeCapTooLow);
    }
    let expected_cost =
        gas.checked_mul(effective_fee).ok_or(PriorityFilterError::ArithmeticOverflow)?;
    let base_cost = gas.checked_mul(base_fee).ok_or(PriorityFilterError::ArithmeticOverflow)?;

    let (break_even_floor, break_even_remainder, break_even_ratio_ppm, max_profitable_priority) =
        if value > base_cost {
            let priority_budget = value - base_cost;
            let floor = priority_budget / gas;
            let remainder = priority_budget % gas;
            let ratio_ppm = priority_budget
                .checked_mul(U256::from(PARTS_PER_MILLION))
                .ok_or(PriorityFilterError::ArithmeticOverflow)?
                / base_cost;
            let maximum = (priority_budget - U256::from(1)) / gas;
            (floor, remainder, ratio_ppm, Some(maximum))
        } else {
            (U256::ZERO, U256::ZERO, U256::ZERO, None)
        };
    let expected_ev = value.checked_sub(expected_cost).filter(|ev| !ev.is_zero());

    Ok(PriorityFilterDecision {
        expected_cost_wei: expected_cost,
        expected_ev_wei: expected_ev,
        break_even_priority_fee_per_gas_wei_floor: break_even_floor,
        break_even_remainder_wei: break_even_remainder,
        break_even_priority_to_base_fee_ppm: break_even_ratio_ppm,
        max_profitable_priority_fee_per_gas_wei: max_profitable_priority,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(value: u64, gas: u64, base: u64, priority: u64) -> PriorityFilterInput {
        PriorityFilterInput {
            candidate_value_wei: Some(U256::from(value)),
            gas_estimate: Some(U256::from(gas)),
            base_fee_per_gas_wei: Some(U256::from(base)),
            victim_max_priority_fee_per_gas_wei: Some(U256::from(priority)),
            victim_max_fee_per_gas_wei: Some(U256::from(base + priority)),
            candidate_block: 42,
            fee_block: 42,
        }
    }

    #[test]
    fn strict_threshold_rejects_below_and_equal_but_admits_above() {
        let below = evaluate(input(109, 10, 5, 6)).unwrap();
        let equal = evaluate(input(110, 10, 5, 6)).unwrap();
        let above = evaluate(input(111, 10, 5, 6)).unwrap();
        assert!(!below.admitted());
        assert!(!equal.admitted());
        assert_eq!(above.expected_ev_wei, Some(U256::from(1)));
        assert_eq!(above.break_even_priority_fee_per_gas_wei_floor, U256::from(6));
        assert_eq!(above.break_even_remainder_wei, U256::from(1));
        assert_eq!(above.break_even_priority_to_base_fee_ppm, U256::from(1_220_000));
        assert_eq!(above.max_profitable_priority_fee_per_gas_wei, Some(U256::from(6)));
    }

    #[test]
    fn explicit_wei_and_gas_units_change_cost_without_scaling_constants() {
        let decision = evaluate(input(42_000, 2_000, 10, 10)).unwrap();
        assert_eq!(decision.expected_cost_wei, U256::from(40_000));
        assert_eq!(decision.expected_ev_wei, Some(U256::from(2_000)));
        assert_eq!(decision.break_even_priority_fee_per_gas_wei_floor, U256::from(11));
        assert_eq!(decision.break_even_priority_to_base_fee_ppm, U256::from(1_100_000));
    }

    #[test]
    fn varying_value_gas_base_fee_and_priority_changes_admission() {
        assert!(evaluate(input(101, 10, 5, 5)).unwrap().admitted());
        assert!(!evaluate(input(100, 10, 5, 5)).unwrap().admitted());
        assert!(!evaluate(input(101, 11, 5, 5)).unwrap().admitted());
        assert!(!evaluate(input(101, 10, 6, 5)).unwrap().admitted());
        assert!(!evaluate(input(101, 10, 5, 6)).unwrap().admitted());
    }

    #[test]
    fn missing_invalid_stale_and_capped_inputs_fail_closed() {
        let mut case = input(101, 10, 5, 5);
        case.victim_max_priority_fee_per_gas_wei = None;
        assert_eq!(evaluate(case), Err(PriorityFilterError::MissingVictimPriorityFee));

        let mut case = input(101, 10, 5, 5);
        case.gas_estimate = None;
        assert_eq!(evaluate(case), Err(PriorityFilterError::MissingGasEstimate));

        let mut case = input(101, 10, 5, 5);
        case.gas_estimate = Some(U256::ZERO);
        assert_eq!(evaluate(case), Err(PriorityFilterError::ZeroGasEstimate));

        let mut case = input(101, 10, 5, 5);
        case.fee_block += 1;
        assert_eq!(evaluate(case), Err(PriorityFilterError::StaleFeeAuthority));

        let mut case = input(101, 10, 5, 5);
        case.victim_max_fee_per_gas_wei = Some(U256::from(9));
        assert_eq!(evaluate(case), Err(PriorityFilterError::VictimFeeCapTooLow));
    }

    #[test]
    fn arithmetic_overflow_fails_closed() {
        let mut case = input(101, 10, 5, 5);
        case.base_fee_per_gas_wei = Some(U256::MAX);
        assert_eq!(evaluate(case), Err(PriorityFilterError::ArithmeticOverflow));

        let mut case = input(101, 10, 5, 5);
        case.gas_estimate = Some(U256::MAX);
        assert_eq!(evaluate(case), Err(PriorityFilterError::ArithmeticOverflow));
        let mut case = input(101, 1, 1, 1);
        case.candidate_value_wei = Some(U256::MAX);
        assert_eq!(evaluate(case), Err(PriorityFilterError::ArithmeticOverflow));
    }
}
