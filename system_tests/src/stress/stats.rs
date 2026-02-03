//! Atomic statistics collection for stress tests.

use std::sync::atomic::{AtomicU64, Ordering};

/// Thread-safe statistics collector for stress test results.
#[derive(Debug, Default)]
pub struct Stats {
    /// Total transactions submitted.
    pub tx_submitted: AtomicU64,
    /// Total transactions that failed to submit.
    pub tx_failed: AtomicU64,
    /// Total transaction receipts obtained.
    pub tx_receipts_obtained: AtomicU64,
}

impl Stats {
    /// Creates a new stats collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a successful transaction submission.
    pub fn record_submitted(&self) {
        self.tx_submitted.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a failed transaction submission.
    pub fn record_failed(&self) {
        self.tx_failed.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a transaction receipt obtained.
    pub fn record_receipt(&self) {
        self.tx_receipts_obtained.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns the number of submitted transactions.
    pub fn submitted(&self) -> u64 {
        self.tx_submitted.load(Ordering::Relaxed)
    }

    /// Returns the number of failed transactions.
    pub fn failed(&self) -> u64 {
        self.tx_failed.load(Ordering::Relaxed)
    }

    /// Returns the number of receipts obtained.
    pub fn receipts(&self) -> u64 {
        self.tx_receipts_obtained.load(Ordering::Relaxed)
    }

    /// Returns the success rate as a value between 0.0 and 1.0.
    pub fn success_rate(&self) -> f64 {
        let submitted = self.submitted();
        if submitted == 0 {
            return 0.0;
        }
        let failed = self.failed();
        (submitted - failed) as f64 / submitted as f64
    }

    /// Asserts that all submitted transactions succeeded.
    pub fn assert_all_succeeded(&self) {
        let submitted = self.submitted();
        let failed = self.failed();
        assert!(submitted > 0, "Expected at least one transaction to be submitted");
        assert_eq!(
            failed, 0,
            "Expected no failed transactions, but {} out of {} failed",
            failed, submitted
        );
    }
}
