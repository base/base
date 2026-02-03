//! Stress test configuration.

/// Configuration for the stress test generator.
#[derive(Clone, Debug)]
pub struct StressConfig {
    /// Transactions per second rate.
    pub tx_rate: f64,
    /// Maximum concurrent in-flight transactions.
    pub parallel: usize,
    /// Test duration in seconds.
    pub duration_secs: u64,
    /// Storage slots to create per transaction.
    pub create_storage: u64,
    /// Accounts to touch per transaction.
    pub create_accounts: u64,
    /// Minimum priority fee in gwei.
    pub priority_fee_min_gwei: f64,
    /// Maximum priority fee in gwei.
    pub priority_fee_max_gwei: f64,
}

impl Default for StressConfig {
    fn default() -> Self {
        Self {
            tx_rate: 2.0,
            parallel: 3,
            duration_secs: 15,
            create_storage: 5,
            create_accounts: 2,
            priority_fee_min_gwei: 0.001,
            priority_fee_max_gwei: 1.0,
        }
    }
}

impl StressConfig {
    /// Sets the transaction rate (TPS).
    pub fn with_tx_rate(mut self, rate: f64) -> Self {
        self.tx_rate = rate;
        self
    }

    /// Sets the maximum parallel in-flight transactions.
    pub fn with_parallel(mut self, n: usize) -> Self {
        self.parallel = n;
        self
    }

    /// Sets the test duration in seconds.
    pub fn with_duration_secs(mut self, secs: u64) -> Self {
        self.duration_secs = secs;
        self
    }

    /// Sets the number of storage slots to create per transaction.
    pub fn with_create_storage(mut self, n: u64) -> Self {
        self.create_storage = n;
        self
    }

    /// Sets the number of accounts to touch per transaction.
    pub fn with_create_accounts(mut self, n: u64) -> Self {
        self.create_accounts = n;
        self
    }

    /// Returns the minimum priority fee in wei.
    pub fn priority_fee_min_wei(&self) -> u128 {
        (self.priority_fee_min_gwei * 1e9) as u128
    }

    /// Returns the maximum priority fee in wei.
    pub fn priority_fee_max_wei(&self) -> u128 {
        (self.priority_fee_max_gwei * 1e9) as u128
    }
}
