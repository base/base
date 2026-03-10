//! Transaction manager configuration.

/// Configuration for the transaction manager.
///
/// Controls fee-limit enforcement behaviour used by
/// [`FeeCalculator::check_limits`](crate::FeeCalculator::check_limits).
#[derive(Debug, Clone)]
pub struct TxManagerConfig {
    /// Maximum allowed multiplier applied to the suggested fee.
    ///
    /// When the suggested fee is at or above [`fee_limit_threshold`](Self::fee_limit_threshold),
    /// a proposed fee that exceeds `fee_limit_multiplier × suggested` is rejected
    /// with [`TxManagerError::FeeLimitExceeded`](crate::TxManagerError::FeeLimitExceeded).
    pub fee_limit_multiplier: u64,

    /// Minimum suggested fee at which the fee-limit check activates.
    ///
    /// If the suggested fee is below this value the limit check is skipped,
    /// allowing unconstrained fees in low-fee environments.
    pub fee_limit_threshold: u128,
}

impl Default for TxManagerConfig {
    fn default() -> Self {
        Self { fee_limit_multiplier: 5, fee_limit_threshold: 0 }
    }
}