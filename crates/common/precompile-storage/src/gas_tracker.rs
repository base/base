//! Gas tracker for Base native precompiles.
//!
//! Ported from `revm-context-interface` v17 (`context_interface::cfg::gas::GasTracker`).
//! Remove this file once Base upgrades to revm ≥ 38 (context-interface ≥ 17), at which
//! point `GasTracker` can be imported directly via `revm::interpreter::gas::GasTracker`.

/// Tracks gas consumption for a single precompile call.
///
/// Regular execution gas and EIP-8037 state-creation gas are tracked separately:
/// - **Regular gas** (`record_regular_cost`): deducted from `remaining`.
/// - **State gas** (`record_state_cost`): deducted from `reservoir` first; when the reservoir
///   is exhausted, remaining state gas spills into `remaining`.
///
/// With `reservoir = 0` (standard mainnet), all gas is regular gas and behavior is identical
/// to a simple checked counter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GasTracker {
    /// Total gas limit for this call.
    gas_limit: u64,
    /// Remaining regular gas.
    remaining: u64,
    /// EIP-8037 state-gas reservoir. Excess state gas spills into `remaining` once exhausted.
    reservoir: u64,
    /// Cumulative state gas spent (zero→nonzero SSTORE, code deposit).
    state_gas_spent: u64,
    /// Accumulated gas refund (EIP-3529).
    refunded: i64,
}

impl GasTracker {
    /// Creates a new tracker.
    ///
    /// For a fresh precompile call: `GasTracker::new(limit, limit, reservoir)`.
    /// Pass `reservoir = 0` for standard (non-EIP-8037) operation.
    #[inline]
    pub const fn new(gas_limit: u64, remaining: u64, reservoir: u64) -> Self {
        Self { gas_limit, remaining, reservoir, state_gas_spent: 0, refunded: 0 }
    }

    /// Returns the gas limit.
    #[inline]
    pub const fn limit(&self) -> u64 {
        self.gas_limit
    }

    /// Returns the remaining regular gas.
    #[inline]
    pub const fn remaining(&self) -> u64 {
        self.remaining
    }

    /// Returns the remaining EIP-8037 state-gas reservoir.
    #[inline]
    pub const fn reservoir(&self) -> u64 {
        self.reservoir
    }

    /// Returns the total state gas spent so far.
    #[inline]
    pub const fn state_gas_spent(&self) -> u64 {
        self.state_gas_spent
    }

    /// Returns the accumulated gas refund.
    #[inline]
    pub const fn refunded(&self) -> i64 {
        self.refunded
    }

    /// Deducts `cost` from the remaining regular gas.
    ///
    /// Returns `false` (out-of-gas) if the remaining balance is insufficient.
    #[inline]
    #[must_use]
    pub fn record_regular_cost(&mut self, cost: u64) -> bool {
        match self.remaining.checked_sub(cost) {
            Some(new_remaining) => {
                self.remaining = new_remaining;
                true
            }
            None => false,
        }
    }

    /// Deducts `cost` from the EIP-8037 state-gas reservoir, spilling into regular gas
    /// once the reservoir is exhausted.
    ///
    /// Returns `false` (out-of-gas) if neither the reservoir nor remaining regular gas can
    /// cover the cost.
    #[inline]
    #[must_use]
    pub fn record_state_cost(&mut self, cost: u64) -> bool {
        if self.reservoir >= cost {
            self.state_gas_spent = self.state_gas_spent.saturating_add(cost);
            self.reservoir -= cost;
            return true;
        }

        let spill = cost - self.reservoir;
        if self.record_regular_cost(spill) {
            self.state_gas_spent = self.state_gas_spent.saturating_add(cost);
            self.reservoir = 0;
            true
        } else {
            false
        }
    }

    /// Accumulates a gas refund (EIP-3529).
    #[inline]
    pub fn record_refund(&mut self, refund: i64) {
        self.refunded = self.refunded.saturating_add(refund);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regular_cost_deducts_from_remaining() {
        let mut t = GasTracker::new(1000, 1000, 0);
        assert!(t.record_regular_cost(400));
        assert_eq!(t.remaining(), 600);
        assert_eq!(t.limit() - t.remaining(), 400);
    }

    #[test]
    fn test_regular_cost_oog() {
        let mut t = GasTracker::new(100, 100, 0);
        assert!(!t.record_regular_cost(101));
        assert_eq!(t.remaining(), 100, "remaining unchanged on OOG");
    }

    #[test]
    fn test_state_cost_deducts_from_reservoir_first() {
        let mut t = GasTracker::new(1000, 1000, 500);
        assert!(t.record_state_cost(300));
        assert_eq!(t.reservoir(), 200);
        assert_eq!(t.remaining(), 1000, "regular gas untouched");
        assert_eq!(t.state_gas_spent(), 300);
    }

    #[test]
    fn test_state_cost_spills_into_regular_when_reservoir_exhausted() {
        let mut t = GasTracker::new(1000, 1000, 100);
        // costs 250: 100 from reservoir, 150 spills into remaining
        assert!(t.record_state_cost(250));
        assert_eq!(t.reservoir(), 0);
        assert_eq!(t.remaining(), 850);
        assert_eq!(t.state_gas_spent(), 250);
    }

    #[test]
    fn test_state_cost_oog_when_spill_exceeds_remaining() {
        let mut t = GasTracker::new(100, 100, 50);
        // costs 200: 50 from reservoir, 150 spill but only 100 remaining
        assert!(!t.record_state_cost(200));
        assert_eq!(t.reservoir(), 50, "reservoir unchanged on OOG");
        assert_eq!(t.remaining(), 100, "remaining unchanged on OOG");
    }

    #[test]
    fn test_refund_accumulates() {
        let mut t = GasTracker::new(1000, 1000, 0);
        t.record_refund(100);
        t.record_refund(200);
        assert_eq!(t.refunded(), 300);
    }

    #[test]
    fn test_zero_reservoir_behaves_like_simple_counter() {
        let mut t = GasTracker::new(500, 500, 0);
        assert!(t.record_regular_cost(200));
        assert_eq!(t.remaining(), 300);
        assert!(!t.record_regular_cost(301));
    }
}
