use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Receipt-observed transactions the calibrated average counts as, so a few early receipts of
/// heavy or light transactions cannot swing pacing.
const CALIBRATION_PRIOR_TRANSACTIONS: u64 = 64;

/// The corrected estimate stays within this factor of the calibrated average either way.
const MAX_CORRECTION_FACTOR: u64 = 4;

/// Per-transaction gas that pacing plans with: the calibrated average, corrected toward the
/// canonical receipt gas of measured transactions.
///
/// Calibration sends each payload type twice on a near-empty chain, which can miss run-time
/// costs such as overwrites of cold storage slots. Every planned transaction carries the same
/// estimate, so the receipt-gas average of measured transactions is the estimate that makes
/// offered gas match on-chain gas. Clones share the current value.
#[derive(Debug, Clone)]
pub struct GasEstimate {
    calibrated: u64,
    current: Arc<AtomicU64>,
}

impl GasEstimate {
    /// Starts from the calibrated average gas per transaction.
    pub fn new(calibrated: u64) -> Self {
        Self { calibrated, current: Arc::new(AtomicU64::new(calibrated)) }
    }

    /// Returns the current per-transaction estimate.
    pub fn get(&self) -> u64 {
        self.current.load(Ordering::Relaxed)
    }

    /// Blends `observed_gas` receipt gas over `observed_count` measured transactions into the
    /// calibrated average, stores the result, and returns it.
    pub fn update(&self, observed_gas: u128, observed_count: u64) -> u64 {
        let prior = u128::from(CALIBRATION_PRIOR_TRANSACTIONS);
        let blended = (u128::from(self.calibrated) * prior).saturating_add(observed_gas)
            / (prior + u128::from(observed_count));
        let estimate = u64::try_from(blended).unwrap_or(u64::MAX).clamp(
            self.calibrated / MAX_CORRECTION_FACTOR,
            self.calibrated.saturating_mul(MAX_CORRECTION_FACTOR),
        );
        self.current.store(estimate, Ordering::Relaxed);
        estimate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::SeededRng;

    #[test]
    fn converges_to_receipt_gas() {
        let estimate = GasEstimate::new(150_000);
        let shared = estimate.clone();
        assert_eq!(estimate.update(0, 0), 150_000);

        let after_one_block = estimate.update(500 * 190_000, 500);
        assert!((185_000..190_000).contains(&after_one_block), "{after_one_block}");
        assert_eq!(estimate.update(50_000 * 190_000, 50_000), 189_948);
        assert_eq!(shared.get(), 189_948, "clones share the estimate");
    }

    #[test]
    fn clamps_to_four_times_calibration() {
        let estimate = GasEstimate::new(100_000);
        assert_eq!(estimate.update(10_000 * 1_000_000, 10_000), 400_000);
        assert_eq!(estimate.update(10_000 * 1_000, 10_000), 25_000);
    }

    #[test]
    fn noisy_receipts_settle_near_the_mean() {
        // Heavy-tailed mix: a 1.2M-gas transaction with probability 0.1, else a 21k transfer;
        // mean 138.9k. Each sample is one 500-transaction block.
        let estimate = GasEstimate::new(100_000);
        let mut rng = SeededRng::new(7);
        let (mut gas, mut count, mut current) = (0u128, 0u64, 0);
        for sample in 0..100 {
            for _ in 0..500 {
                gas += if rng.random::<f64>() < 0.1 { 1_200_000 } else { 21_000 };
                count += 1;
            }
            current = estimate.update(gas, count);
            if sample >= 20 {
                assert!((132_000..=145_800).contains(&current), "sample {sample}: {current}");
            }
        }
        assert!((136_100..=141_700).contains(&current), "final: {current}");
    }
}
