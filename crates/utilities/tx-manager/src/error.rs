//! Transaction manager error types.

use thiserror::Error;

/// Transaction manager error types.
///
/// Variants are grouped into critical (non-retryable), fee/replacement
/// (retryable via fee bumps), and infrastructure (retryable/transient) errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TxManagerError {
    // ── Critical errors (non-retryable) ──────────────────────────────────
    /// Nonce already consumed onchain.
    #[error("nonce too low")]
    NonceTooLow,

    /// Nonce too far ahead of chain state.
    #[error("nonce too high")]
    NonceTooHigh,

    /// Account balance cannot cover gas + value.
    #[error("insufficient funds")]
    InsufficientFunds,

    /// Gas limit below intrinsic gas cost.
    #[error("intrinsic gas too low")]
    IntrinsicGasTooLow,

    /// EVM execution reverted.
    #[error("execution reverted")]
    ExecutionReverted,

    /// Mempool inclusion deadline expired.
    #[error("mempool deadline expired")]
    MempoolDeadlineExpired,

    /// Nonce slot was already reserved.
    #[error("nonce already reserved")]
    AlreadyReserved,

    /// Nonce arithmetic overflowed `u64::MAX`.
    #[error("nonce overflow")]
    NonceOverflow,

    /// Send response channel closed before a result was delivered.
    ///
    /// The background send task exited (panicked or was cancelled)
    /// before completing. Non-retryable.
    #[error("send response channel closed")]
    ChannelClosed,

    /// Calculated fee exceeds the configured fee-limit ceiling.
    ///
    /// Returned by [`FeeCalculator::check_limits`] when the proposed fee
    /// surpasses `fee_limit_multiplier × suggested_fee` and the suggested
    /// fee is at or above `fee_limit_threshold`. Non-retryable.
    #[error("fee limit exceeded: fee {fee} exceeds ceiling {ceiling}")]
    FeeLimitExceeded {
        /// The proposed fee that was rejected.
        fee: u128,
        /// The ceiling that was exceeded (`fee_limit_multiplier × suggested`).
        ceiling: u128,
    },

    /// The `safe_abort_nonce_too_low_count` threshold was set to zero.
    ///
    /// A zero threshold would cause the send loop to abort on the very first
    /// nonce-too-low error after a successful publish, making fee bumps
    /// impossible.
    #[error("invalid safe_abort_nonce_too_low_count: must be greater than 0")]
    InvalidSafeAbortNonceTooLowCount,

    // ── Fee / replacement errors (retryable) ─────────────────────────────
    /// Fee too low to enter the mempool.
    #[error("transaction underpriced")]
    Underpriced,

    /// Replacement transaction fee bump insufficient.
    #[error("replacement transaction underpriced")]
    ReplacementUnderpriced,

    /// Generic fee rejection.
    #[error("fee too low")]
    FeeTooLow,

    /// `maxFeePerGas` below block base fee.
    #[error("max fee per gas less than block base fee")]
    MaxFeePerGasTooLow,

    // ── Infrastructure / transient errors (retryable) ────────────────────
    /// Transaction already present in the mempool (benign on resubmission).
    #[error("transaction already known")]
    AlreadyKnown,

    /// Unclassified RPC error preserving the original error string.
    ///
    /// This variant is treated as retryable by [`TxManagerError::is_retryable`]
    /// because unknown errors may be transient. Callers **must** enforce bounded
    /// retry counts and exponential backoff to prevent retry storms from
    /// persistent, non-transient errors that happen to be unclassified.
    #[error("rpc error: {0}")]
    Rpc(String),
}

impl TxManagerError {
    /// Returns `true` if this error is transient or can be resolved by
    /// bumping fees, meaning the send loop should retry.
    ///
    /// Fee/replacement errors and infrastructure errors are retryable.
    /// Critical errors (nonce conflicts, insufficient funds, reverts,
    /// deadline expiry, reservation conflicts) are not.
    ///
    /// # Caller requirements
    ///
    /// The [`Rpc`](Self::Rpc) fallback is conservatively treated as retryable.
    /// Callers **must** enforce a maximum retry count with exponential backoff
    /// to avoid unbounded retries on persistent, non-transient errors that are
    /// unrecognized by [`RpcErrorClassifier::classify_rpc_error`].
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Underpriced
                | Self::ReplacementUnderpriced
                | Self::FeeTooLow
                | Self::MaxFeePerGasTooLow
                | Self::AlreadyKnown
                | Self::Rpc(_)
        )
    }

    /// Returns `true` only for [`TxManagerError::AlreadyKnown`].
    ///
    /// The send loop uses this to distinguish "already in mempool" (a
    /// success on resubmission) from actual errors.
    #[must_use]
    pub const fn is_already_known(&self) -> bool {
        matches!(self, Self::AlreadyKnown)
    }
}

/// Result type alias for transaction manager operations.
pub type TxManagerResult<T> = Result<T, TxManagerError>;

/// Classifies raw RPC error strings into structured [`TxManagerError`] variants.
///
/// This mirrors the Go `op-service/txmgr` `errStringMatch` approach, enabling
/// the send loop to make retry/abort decisions based on error type.
///
/// # Limitations
///
/// Classification relies on substring matching against known geth error
/// messages. Other Ethereum clients (Erigon, Besu, Nethermind) may use
/// different wording for equivalent errors, causing them to fall through to
/// the [`TxManagerError::Rpc`] fallback. Future improvements could augment
/// string matching with JSON-RPC error codes (e.g., `-32000`) for more
/// robust cross-client classification.
#[derive(Debug)]
pub struct RpcErrorClassifier;

impl RpcErrorClassifier {
    /// Classifies a raw RPC error message into a [`TxManagerError`] variant.
    ///
    /// The input is lowercased once, then checked against known geth error
    /// substrings in a fixed order. The first match wins.
    ///
    /// **Ordering is critical**: `"replacement transaction underpriced"` is
    /// matched before `"transaction underpriced"` because the latter is a
    /// substring of the former.
    ///
    /// Unknown error strings fall through to [`TxManagerError::Rpc`],
    /// preserving the original casing.
    #[must_use]
    pub fn classify_rpc_error(error_msg: &str) -> TxManagerError {
        let lowered = error_msg.to_lowercase();

        if lowered.contains("replacement transaction underpriced") {
            return TxManagerError::ReplacementUnderpriced;
        }
        if lowered.contains("transaction underpriced") {
            return TxManagerError::Underpriced;
        }
        if lowered.contains("nonce too low") {
            return TxManagerError::NonceTooLow;
        }
        if lowered.contains("nonce too high") {
            return TxManagerError::NonceTooHigh;
        }
        if lowered.contains("insufficient funds") {
            return TxManagerError::InsufficientFunds;
        }
        if lowered.contains("intrinsic gas too low") {
            return TxManagerError::IntrinsicGasTooLow;
        }
        if lowered.contains("execution reverted") {
            return TxManagerError::ExecutionReverted;
        }
        if lowered.contains("fee too low") {
            return TxManagerError::FeeTooLow;
        }
        if lowered.contains("max fee per gas less than block base fee") {
            return TxManagerError::MaxFeePerGasTooLow;
        }
        if lowered.contains("already known") {
            return TxManagerError::AlreadyKnown;
        }
        if lowered.contains("transaction already in pool") {
            return TxManagerError::AlreadyKnown;
        }

        TxManagerError::Rpc(error_msg.to_string())
    }

    /// Returns `true` if `error_msg` contains any of the given substrings
    /// (compared case-insensitively).
    ///
    /// This enables callers to define custom error matching sets beyond the
    /// built-in [`RpcErrorClassifier::classify_rpc_error`] classification.
    #[must_use]
    pub fn err_string_contains_any(error_msg: &str, substrings: &[&str]) -> bool {
        let lowered = error_msg.to_lowercase();
        substrings.iter().any(|s| lowered.contains(&s.to_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    // ── classify_rpc_error ───────────────────────────────────────────────

    #[rstest]
    #[case::replacement_underpriced(
        "replacement transaction underpriced",
        TxManagerError::ReplacementUnderpriced
    )]
    #[case::underpriced("transaction underpriced", TxManagerError::Underpriced)]
    #[case::nonce_too_low("nonce too low", TxManagerError::NonceTooLow)]
    #[case::nonce_too_high("nonce too high", TxManagerError::NonceTooHigh)]
    #[case::insufficient_funds("insufficient funds", TxManagerError::InsufficientFunds)]
    #[case::intrinsic_gas_too_low("intrinsic gas too low", TxManagerError::IntrinsicGasTooLow)]
    #[case::execution_reverted("execution reverted", TxManagerError::ExecutionReverted)]
    #[case::fee_too_low("fee too low", TxManagerError::FeeTooLow)]
    #[case::max_fee_too_low(
        "max fee per gas less than block base fee",
        TxManagerError::MaxFeePerGasTooLow
    )]
    #[case::already_known("already known", TxManagerError::AlreadyKnown)]
    #[case::already_in_pool("transaction already in pool", TxManagerError::AlreadyKnown)]
    #[case::case_insensitive_upper("NONCE TOO LOW", TxManagerError::NonceTooLow)]
    #[case::case_insensitive_mixed("Nonce Too Low", TxManagerError::NonceTooLow)]
    #[case::substring_in_context(
        "some context: nonce too low for account",
        TxManagerError::NonceTooLow
    )]
    #[case::unknown_fallback("something unexpected", TxManagerError::Rpc("something unexpected".to_string()))]
    #[case::preserves_casing("Some Unknown ERROR", TxManagerError::Rpc("Some Unknown ERROR".to_string()))]
    #[case::empty_string("", TxManagerError::Rpc(String::new()))]
    #[case::mempool_deadline_not_classified("mempool deadline expired", TxManagerError::Rpc("mempool deadline expired".to_string()))]
    #[case::already_reserved_not_classified("nonce already reserved", TxManagerError::Rpc("nonce already reserved".to_string()))]
    fn classify_rpc_error(#[case] input: &str, #[case] expected: TxManagerError) {
        assert_eq!(RpcErrorClassifier::classify_rpc_error(input), expected);
    }

    // ── is_retryable ────────────────────────────────────────────────────

    #[rstest]
    #[case::nonce_too_low(TxManagerError::NonceTooLow, false)]
    #[case::nonce_too_high(TxManagerError::NonceTooHigh, false)]
    #[case::insufficient_funds(TxManagerError::InsufficientFunds, false)]
    #[case::intrinsic_gas_too_low(TxManagerError::IntrinsicGasTooLow, false)]
    #[case::execution_reverted(TxManagerError::ExecutionReverted, false)]
    #[case::mempool_deadline(TxManagerError::MempoolDeadlineExpired, false)]
    #[case::already_reserved(TxManagerError::AlreadyReserved, false)]
    #[case::channel_closed(TxManagerError::ChannelClosed, false)]
    #[case::fee_limit_exceeded(TxManagerError::FeeLimitExceeded { fee: 0, ceiling: 0 }, false)]
    #[case::invalid_safe_abort(TxManagerError::InvalidSafeAbortNonceTooLowCount, false)]
    #[case::nonce_overflow(TxManagerError::NonceOverflow, false)]
    #[case::underpriced(TxManagerError::Underpriced, true)]
    #[case::replacement_underpriced(TxManagerError::ReplacementUnderpriced, true)]
    #[case::fee_too_low(TxManagerError::FeeTooLow, true)]
    #[case::max_fee_too_low(TxManagerError::MaxFeePerGasTooLow, true)]
    #[case::already_known(TxManagerError::AlreadyKnown, true)]
    #[case::rpc(TxManagerError::Rpc("any error".to_string()), true)]
    fn is_retryable(#[case] error: TxManagerError, #[case] expected: bool) {
        assert_eq!(error.is_retryable(), expected);
    }

    // ── is_already_known ────────────────────────────────────────────────

    #[rstest]
    #[case::already_known(TxManagerError::AlreadyKnown, true)]
    #[case::nonce_too_low(TxManagerError::NonceTooLow, false)]
    #[case::underpriced(TxManagerError::Underpriced, false)]
    #[case::rpc_with_already_known_text(TxManagerError::Rpc("already known".to_string()), false)]
    #[case::channel_closed(TxManagerError::ChannelClosed, false)]
    #[case::invalid_safe_abort(TxManagerError::InvalidSafeAbortNonceTooLowCount, false)]
    fn is_already_known(#[case] error: TxManagerError, #[case] expected: bool) {
        assert_eq!(error.is_already_known(), expected);
    }

    // ── Display output ──────────────────────────────────────────────────

    #[rstest]
    #[case::nonce_too_low(TxManagerError::NonceTooLow, "nonce too low")]
    #[case::nonce_too_high(TxManagerError::NonceTooHigh, "nonce too high")]
    #[case::insufficient_funds(TxManagerError::InsufficientFunds, "insufficient funds")]
    #[case::intrinsic_gas_too_low(TxManagerError::IntrinsicGasTooLow, "intrinsic gas too low")]
    #[case::execution_reverted(TxManagerError::ExecutionReverted, "execution reverted")]
    #[case::mempool_deadline(TxManagerError::MempoolDeadlineExpired, "mempool deadline expired")]
    #[case::already_reserved(TxManagerError::AlreadyReserved, "nonce already reserved")]
    #[case::underpriced(TxManagerError::Underpriced, "transaction underpriced")]
    #[case::replacement_underpriced(
        TxManagerError::ReplacementUnderpriced,
        "replacement transaction underpriced"
    )]
    #[case::fee_too_low(TxManagerError::FeeTooLow, "fee too low")]
    #[case::max_fee_too_low(
        TxManagerError::MaxFeePerGasTooLow,
        "max fee per gas less than block base fee"
    )]
    #[case::already_known(TxManagerError::AlreadyKnown, "transaction already known")]
    #[case::channel_closed(TxManagerError::ChannelClosed, "send response channel closed")]
    #[case::fee_limit_exceeded(TxManagerError::FeeLimitExceeded { fee: 501, ceiling: 500 }, "fee limit exceeded: fee 501 exceeds ceiling 500")]
    #[case::invalid_safe_abort(
        TxManagerError::InvalidSafeAbortNonceTooLowCount,
        "invalid safe_abort_nonce_too_low_count: must be greater than 0"
    )]
    #[case::nonce_overflow(TxManagerError::NonceOverflow, "nonce overflow")]
    #[case::rpc(TxManagerError::Rpc("test".to_string()), "rpc error: test")]
    fn display_output(#[case] error: TxManagerError, #[case] expected: &str) {
        assert_eq!(error.to_string(), expected);
    }

    // ── err_string_contains_any ─────────────────────────────────────────

    #[rstest]
    #[case::positive_match("nonce too low", &["nonce too low", "insufficient funds"], true)]
    #[case::no_match("something else", &["nonce too low", "insufficient funds"], false)]
    #[case::empty_slice("nonce too low", &[], false)]
    #[case::partial_substring("error: nonce too low for account 0x123", &["nonce too low"], true)]
    #[case::case_insensitive("NONCE TOO LOW", &["nonce too low"], true)]
    fn err_string_contains_any(
        #[case] input: &str,
        #[case] substrings: &[&str],
        #[case] expected: bool,
    ) {
        assert_eq!(RpcErrorClassifier::err_string_contains_any(input, substrings), expected);
    }
}
