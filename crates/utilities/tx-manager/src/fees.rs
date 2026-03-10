//! Fee calculation and bumping logic.

use alloy_primitives::U256;

use crate::TxManagerError;

/// Calculates and bumps transaction fees.
///
/// All methods are pure, deterministic arithmetic with no I/O.  They operate
/// on `u128` values (alloy's native fee type) and use saturating arithmetic
/// to prevent overflow panics on extreme inputs.
#[derive(Debug)]
pub struct FeeCalculator;

impl FeeCalculator {
    /// Computes the EIP-1559 gas fee cap: `tip + 2 × base_fee`.
    ///
    /// The `2 × base_fee` headroom accommodates one full base-fee doubling
    /// between the time a fee is estimated and the block that includes the
    /// transaction.
    ///
    /// Uses saturating arithmetic — the result is capped at [`u128::MAX`]
    /// rather than panicking on overflow.
    #[must_use]
    pub const fn calc_gas_fee_cap(base_fee: u128, tip: u128) -> u128 {
        tip.saturating_add(base_fee.saturating_mul(2))
    }

    /// Computes the blob fee cap: `2 × blob_base_fee`.
    ///
    /// Mirrors [`calc_gas_fee_cap`](Self::calc_gas_fee_cap) for EIP-4844
    /// blob transactions, providing headroom for one full blob-base-fee
    /// doubling.
    ///
    /// Uses saturating arithmetic — the result is capped at [`u128::MAX`]
    /// rather than panicking on overflow.
    #[must_use]
    pub const fn calc_blob_fee_cap(blob_base_fee: u128) -> u128 {
        blob_base_fee.saturating_mul(2)
    }

    /// Returns the minimum replacement value that satisfies geth's
    /// tx-replacement rules.
    ///
    /// * **Regular transactions** (`is_blob = false`): 10 % bump →
    ///   `x + x / 10`, but always at least `x + 1` when `x > 0`.
    /// * **Blob transactions** (`is_blob = true`): 100 % bump →
    ///   `x + x` (i.e. `2 × x`), but always at least `x + 1` when `x > 0`.
    /// * When `x == 0`, returns `0` (there is no fee to bump).
    ///
    /// The "at least +1" rule prevents stuck bump loops where
    /// `x / 10 == 0` for small values.
    ///
    /// Uses saturating arithmetic — the result is capped at [`u128::MAX`]
    /// rather than panicking on overflow.
    #[must_use]
    pub fn calc_threshold_value(x: u128, is_blob: bool) -> u128 {
        if x == 0 {
            return 0;
        }

        let bump = if is_blob {
            // 100 % bump: x + x = 2x
            x
        } else {
            // 10 % bump: x / 10, but at least 1
            (x / 10).max(1)
        };

        x.saturating_add(bump)
    }

    /// Selects final `(tip, fee_cap)` values that satisfy geth's replacement
    /// rules while preferring fresher network estimates.
    ///
    /// Four cases are evaluated:
    ///
    /// 1. **Both above threshold** — use the new tip and recalculated fee cap.
    /// 2. **Tip above, fee cap below** — use the new tip but keep the
    ///    threshold fee cap.
    /// 3. **Fee cap above, tip below** — use the threshold tip and
    ///    recalculate the fee cap from the threshold tip.
    /// 4. **Both below** — use both threshold values.
    ///
    /// The returned fee cap always reflects the tip that was selected so that
    /// the EIP-1559 relationship `fee_cap >= tip` is maintained.
    #[must_use]
    pub fn update_fees(
        old_tip: u128,
        old_fee_cap: u128,
        new_tip: u128,
        new_base_fee: u128,
        is_blob: bool,
    ) -> (u128, u128) {
        let threshold_tip = Self::calc_threshold_value(old_tip, is_blob);
        let threshold_fee_cap = Self::calc_threshold_value(old_fee_cap, is_blob);

        let new_fee_cap = Self::calc_gas_fee_cap(new_base_fee, new_tip);

        let tip_above = new_tip >= threshold_tip;
        let cap_above = new_fee_cap >= threshold_fee_cap;

        match (tip_above, cap_above) {
            // Case 1: both above threshold → use new values
            (true, true) => (new_tip, new_fee_cap),
            // Case 2: tip above, fee cap below → new tip + threshold fee cap
            (true, false) => (new_tip, threshold_fee_cap),
            // Case 3: fee cap above, tip below → threshold tip + recalculated fee cap
            (false, true) => {
                let recalculated = Self::calc_gas_fee_cap(new_base_fee, threshold_tip);
                (threshold_tip, recalculated)
            }
            // Case 4: both below → both threshold values
            (false, false) => (threshold_tip, threshold_fee_cap),
        }
    }

    /// Enforces a configurable fee ceiling.
    ///
    /// * If `suggested < fee_limit_threshold`, the check is **skipped**
    ///   (returns `Ok(())`), allowing unconstrained fees in low-fee
    ///   environments.
    /// * If `fee > fee_limit_multiplier × suggested` (and the threshold is
    ///   met), returns [`TxManagerError::FeeLimitExceeded`].
    /// * Otherwise returns `Ok(())`.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::FeeLimitExceeded`] when the proposed fee
    /// exceeds the configured ceiling.
    pub const fn check_limits(
        fee: u128,
        suggested: u128,
        fee_limit_multiplier: u64,
        fee_limit_threshold: u128,
    ) -> Result<(), TxManagerError> {
        if suggested < fee_limit_threshold {
            return Ok(());
        }

        let ceiling = (fee_limit_multiplier as u128).saturating_mul(suggested);
        if fee > ceiling {
            return Err(TxManagerError::FeeLimitExceeded);
        }

        Ok(())
    }

    /// Enforces a configurable fee ceiling for blob fees.
    ///
    /// Identical logic to [`check_limits`](Self::check_limits) applied to
    /// blob-specific fee values.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::FeeLimitExceeded`] when the proposed blob
    /// fee exceeds the configured ceiling.
    pub const fn check_blob_fee_limits(
        blob_fee: u128,
        suggested_blob_fee: u128,
        fee_limit_multiplier: u64,
        fee_limit_threshold: u128,
    ) -> Result<(), TxManagerError> {
        Self::check_limits(blob_fee, suggested_blob_fee, fee_limit_multiplier, fee_limit_threshold)
    }
}

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
    use proptest::prelude::*;
    use rstest::rstest;

    use super::*;

    // ── GasPriceCaps ────────────────────────────────────────────────────

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

    // ── calc_gas_fee_cap ────────────────────────────────────────────────

    #[rstest]
    #[case::zero_inputs(0, 0, 0)]
    #[case::tip_only(0, 100, 100)]
    #[case::base_only(100, 0, 200)]
    #[case::normal(50, 10, 110)]
    #[case::large_values(1_000_000_000, 500_000_000, 2_500_000_000)]
    #[case::saturates_on_overflow(u128::MAX, 1, u128::MAX)]
    #[case::saturates_both_max(u128::MAX, u128::MAX, u128::MAX)]
    fn calc_gas_fee_cap(#[case] base_fee: u128, #[case] tip: u128, #[case] expected: u128) {
        assert_eq!(FeeCalculator::calc_gas_fee_cap(base_fee, tip), expected);
    }

    // ── calc_blob_fee_cap ───────────────────────────────────────────────

    #[rstest]
    #[case::zero(0, 0)]
    #[case::normal(100, 200)]
    #[case::large(1_000_000_000, 2_000_000_000)]
    #[case::saturates(u128::MAX, u128::MAX)]
    #[case::half_max(u128::MAX / 2 + 1, u128::MAX)]
    fn calc_blob_fee_cap(#[case] blob_base_fee: u128, #[case] expected: u128) {
        assert_eq!(FeeCalculator::calc_blob_fee_cap(blob_base_fee), expected);
    }

    // ── calc_threshold_value ────────────────────────────────────────────

    #[rstest]
    // Regular (non-blob) cases
    #[case::zero_regular(0, false, 0)]
    #[case::one_regular(1, false, 2)]
    #[case::nine_regular(9, false, 10)]
    #[case::ten_regular(10, false, 11)]
    #[case::eleven_regular(11, false, 12)]
    #[case::hundred_regular(100, false, 110)]
    #[case::thousand_regular(1000, false, 1100)]
    #[case::small_value_ensures_plus_one(5, false, 6)]
    #[case::saturates_regular(u128::MAX, false, u128::MAX)]
    // Blob cases
    #[case::zero_blob(0, true, 0)]
    #[case::one_blob(1, true, 2)]
    #[case::ten_blob(10, true, 20)]
    #[case::hundred_blob(100, true, 200)]
    #[case::saturates_blob(u128::MAX, true, u128::MAX)]
    #[case::half_max_blob(u128::MAX / 2 + 1, true, u128::MAX)]
    fn calc_threshold_value(#[case] x: u128, #[case] is_blob: bool, #[case] expected: u128) {
        assert_eq!(FeeCalculator::calc_threshold_value(x, is_blob), expected);
    }

    // ── update_fees ─────────────────────────────────────────────────────

    #[rstest]
    // Case 1: both above threshold → use new values
    #[case::both_above(100, 1000, 200, 500, false, (200, 1200))]
    // Case 2: tip above, fee cap below → new tip + threshold fee cap
    #[case::tip_above_cap_below(100, 1000, 200, 1, false, (200, 1100))]
    // Case 3: fee cap above, tip below → threshold tip + recalculated fee cap
    #[case::tip_below_cap_above(100, 1000, 50, 5000, false, (110, 10110))]
    // Case 4: both below → both threshold values
    #[case::both_below(100, 1000, 50, 1, false, (110, 1100))]
    // Blob cases
    #[case::blob_both_above(100, 1000, 300, 1000, true, (300, 2300))]
    #[case::blob_both_below(100, 1000, 50, 1, true, (200, 2000))]
    // Zero starting fees
    #[case::zero_old_fees(0, 0, 10, 100, false, (10, 210))]
    fn update_fees(
        #[case] old_tip: u128,
        #[case] old_fee_cap: u128,
        #[case] new_tip: u128,
        #[case] new_base_fee: u128,
        #[case] is_blob: bool,
        #[case] expected: (u128, u128),
    ) {
        assert_eq!(
            FeeCalculator::update_fees(old_tip, old_fee_cap, new_tip, new_base_fee, is_blob),
            expected,
        );
    }

    // ── check_limits ────────────────────────────────────────────────────

    #[rstest]
    // Below threshold → skip check, always Ok
    #[case::below_threshold_allows_any(1_000_000, 50, 5, 100, true)]
    // At threshold, within limit → Ok
    #[case::at_threshold_within_limit(400, 100, 5, 100, true)]
    #[case::exactly_at_ceiling(500, 100, 5, 100, true)]
    // At threshold, over limit → Err
    #[case::over_limit(501, 100, 5, 100, false)]
    #[case::way_over_limit(10_000, 100, 5, 100, false)]
    // Zero threshold (always check)
    #[case::zero_threshold_within(5, 1, 5, 0, true)]
    #[case::zero_threshold_over(6, 1, 5, 0, false)]
    // Zero suggested with zero threshold → ceiling is 0
    #[case::zero_suggested_zero_fee(0, 0, 5, 0, true)]
    #[case::zero_suggested_nonzero_fee(1, 0, 5, 0, false)]
    // Large multiplier
    #[case::large_multiplier(u128::MAX, u128::MAX, u64::MAX, 0, true)]
    fn check_limits(
        #[case] fee: u128,
        #[case] suggested: u128,
        #[case] multiplier: u64,
        #[case] threshold: u128,
        #[case] should_pass: bool,
    ) {
        let result = FeeCalculator::check_limits(fee, suggested, multiplier, threshold);
        assert_eq!(result.is_ok(), should_pass);
        if !should_pass {
            assert_eq!(result.unwrap_err(), TxManagerError::FeeLimitExceeded);
        }
    }

    // ── check_blob_fee_limits ───────────────────────────────────────────

    #[rstest]
    #[case::within_limit(400, 100, 5, 100, true)]
    #[case::over_limit(501, 100, 5, 100, false)]
    #[case::below_threshold(1000, 50, 5, 100, true)]
    fn check_blob_fee_limits(
        #[case] blob_fee: u128,
        #[case] suggested: u128,
        #[case] multiplier: u64,
        #[case] threshold: u128,
        #[case] should_pass: bool,
    ) {
        let result =
            FeeCalculator::check_blob_fee_limits(blob_fee, suggested, multiplier, threshold);
        assert_eq!(result.is_ok(), should_pass);
    }

    // ── Property tests ──────────────────────────────────────────────────

    proptest! {
        #[test]
        fn threshold_always_gte_input(x: u128, is_blob: bool) {
            let result = FeeCalculator::calc_threshold_value(x, is_blob);
            prop_assert!(result >= x, "threshold {result} < input {x}");
        }

        #[test]
        fn threshold_forward_progress(x in 1..u128::MAX, is_blob: bool) {
            let result = FeeCalculator::calc_threshold_value(x, is_blob);
            prop_assert!(result > x, "threshold {result} did not advance past {x}");
        }

        #[test]
        fn gas_fee_cap_gte_tip(base_fee: u128, tip: u128) {
            let result = FeeCalculator::calc_gas_fee_cap(base_fee, tip);
            prop_assert!(result >= tip, "fee cap {result} < tip {tip}");
        }

        #[test]
        fn blob_fee_cap_gte_input(blob_base_fee: u128) {
            let result = FeeCalculator::calc_blob_fee_cap(blob_base_fee);
            prop_assert!(result >= blob_base_fee, "blob fee cap {result} < input {blob_base_fee}");
        }

        #[test]
        fn update_fees_tip_meets_threshold(
            old_tip in 0..u64::MAX as u128,
            old_fee_cap in 0..u64::MAX as u128,
            new_tip in 0..u64::MAX as u128,
            new_base_fee in 0..u64::MAX as u128,
            is_blob: bool,
        ) {
            let (final_tip, _) = FeeCalculator::update_fees(
                old_tip, old_fee_cap, new_tip, new_base_fee, is_blob,
            );
            let threshold_tip = FeeCalculator::calc_threshold_value(old_tip, is_blob);
            prop_assert!(
                final_tip >= threshold_tip,
                "final tip {final_tip} < threshold tip {threshold_tip}",
            );
        }

        #[test]
        fn update_fees_fee_cap_meets_threshold(
            old_tip in 0..u64::MAX as u128,
            old_fee_cap in 0..u64::MAX as u128,
            new_tip in 0..u64::MAX as u128,
            new_base_fee in 0..u64::MAX as u128,
            is_blob: bool,
        ) {
            let (_, final_fee_cap) = FeeCalculator::update_fees(
                old_tip, old_fee_cap, new_tip, new_base_fee, is_blob,
            );
            let threshold_fee_cap = FeeCalculator::calc_threshold_value(old_fee_cap, is_blob);
            prop_assert!(
                final_fee_cap >= threshold_fee_cap,
                "final fee cap {final_fee_cap} < threshold fee cap {threshold_fee_cap}",
            );
        }

        #[test]
        fn no_panic_on_extreme_calc_gas_fee_cap(base_fee: u128, tip: u128) {
            let _ = FeeCalculator::calc_gas_fee_cap(base_fee, tip);
        }

        #[test]
        fn no_panic_on_extreme_threshold(x: u128, is_blob: bool) {
            let _ = FeeCalculator::calc_threshold_value(x, is_blob);
        }

        #[test]
        fn no_panic_on_extreme_update_fees(
            old_tip: u128,
            old_fee_cap: u128,
            new_tip: u128,
            new_base_fee: u128,
            is_blob: bool,
        ) {
            let _ = FeeCalculator::update_fees(
                old_tip, old_fee_cap, new_tip, new_base_fee, is_blob,
            );
        }
    }
}
