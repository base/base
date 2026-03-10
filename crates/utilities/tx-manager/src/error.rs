//! Transaction manager error types.

use thiserror::Error;

/// Transaction manager error types.
///
/// Variants are grouped into critical (non-retryable), fee/replacement
/// (retryable via fee bumps), and infrastructure (retryable/transient) errors.
#[derive(Debug, Error)]
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
    use super::*;

    // ── classify_rpc_error: known geth error strings ─────────────────────

    #[test]
    fn classify_replacement_underpriced() {
        let err = RpcErrorClassifier::classify_rpc_error("replacement transaction underpriced");
        assert!(
            matches!(err, TxManagerError::ReplacementUnderpriced),
            "must return ReplacementUnderpriced, not Underpriced — ordering matters"
        );
    }

    #[test]
    fn classify_underpriced() {
        let err = RpcErrorClassifier::classify_rpc_error("transaction underpriced");
        assert!(matches!(err, TxManagerError::Underpriced));
    }

    #[test]
    fn classify_nonce_too_low() {
        let err = RpcErrorClassifier::classify_rpc_error("nonce too low");
        assert!(matches!(err, TxManagerError::NonceTooLow));
    }

    #[test]
    fn classify_nonce_too_high() {
        let err = RpcErrorClassifier::classify_rpc_error("nonce too high");
        assert!(matches!(err, TxManagerError::NonceTooHigh));
    }

    #[test]
    fn classify_insufficient_funds() {
        let err = RpcErrorClassifier::classify_rpc_error("insufficient funds");
        assert!(matches!(err, TxManagerError::InsufficientFunds));
    }

    #[test]
    fn classify_intrinsic_gas_too_low() {
        let err = RpcErrorClassifier::classify_rpc_error("intrinsic gas too low");
        assert!(matches!(err, TxManagerError::IntrinsicGasTooLow));
    }

    #[test]
    fn classify_execution_reverted() {
        let err = RpcErrorClassifier::classify_rpc_error("execution reverted");
        assert!(matches!(err, TxManagerError::ExecutionReverted));
    }

    #[test]
    fn classify_fee_too_low() {
        let err = RpcErrorClassifier::classify_rpc_error("fee too low");
        assert!(matches!(err, TxManagerError::FeeTooLow));
    }

    #[test]
    fn classify_max_fee_per_gas_too_low() {
        let err =
            RpcErrorClassifier::classify_rpc_error("max fee per gas less than block base fee");
        assert!(matches!(err, TxManagerError::MaxFeePerGasTooLow));
    }

    #[test]
    fn classify_already_known() {
        let err = RpcErrorClassifier::classify_rpc_error("already known");
        assert!(matches!(err, TxManagerError::AlreadyKnown));
    }

    #[test]
    fn classify_transaction_already_in_pool() {
        let err = RpcErrorClassifier::classify_rpc_error("transaction already in pool");
        assert!(matches!(err, TxManagerError::AlreadyKnown));
    }

    // ── Fallback to Rpc variant ──────────────────────────────────────────

    #[test]
    fn classify_unknown_returns_rpc_fallback() {
        let msg = "something unexpected";
        let err = RpcErrorClassifier::classify_rpc_error(msg);
        assert!(matches!(err, TxManagerError::Rpc(ref s) if s == msg));
    }

    #[test]
    fn classify_rpc_preserves_original_casing() {
        let msg = "Some Unknown ERROR Message";
        let err = RpcErrorClassifier::classify_rpc_error(msg);
        assert!(matches!(err, TxManagerError::Rpc(ref s) if s == msg));
    }

    // ── Case-insensitivity ───────────────────────────────────────────────

    #[test]
    fn classify_case_insensitive_upper() {
        let err = RpcErrorClassifier::classify_rpc_error("NONCE TOO LOW");
        assert!(matches!(err, TxManagerError::NonceTooLow));
    }

    #[test]
    fn classify_case_insensitive_mixed() {
        let err = RpcErrorClassifier::classify_rpc_error("Nonce Too Low");
        assert!(matches!(err, TxManagerError::NonceTooLow));
    }

    // ── Substring containment in context ─────────────────────────────────

    #[test]
    fn classify_substring_in_longer_message() {
        let err = RpcErrorClassifier::classify_rpc_error("some context: nonce too low for account");
        assert!(matches!(err, TxManagerError::NonceTooLow));
    }

    // ── err_string_contains_any ──────────────────────────────────────────

    #[test]
    fn err_string_contains_any_positive_match() {
        assert!(RpcErrorClassifier::err_string_contains_any(
            "nonce too low",
            &["nonce too low", "insufficient funds"]
        ));
    }

    #[test]
    fn err_string_contains_any_no_match() {
        assert!(!RpcErrorClassifier::err_string_contains_any(
            "something else",
            &["nonce too low", "insufficient funds"]
        ));
    }

    #[test]
    fn err_string_contains_any_empty_slice() {
        assert!(!RpcErrorClassifier::err_string_contains_any("nonce too low", &[]));
    }

    #[test]
    fn err_string_contains_any_partial_substring() {
        assert!(RpcErrorClassifier::err_string_contains_any(
            "error: nonce too low for account 0x123",
            &["nonce too low"]
        ));
    }

    #[test]
    fn err_string_contains_any_case_insensitive() {
        assert!(RpcErrorClassifier::err_string_contains_any("NONCE TOO LOW", &["nonce too low"]));
    }

    // ── is_retryable ─────────────────────────────────────────────────────

    #[test]
    fn is_retryable_critical_errors() {
        assert!(!TxManagerError::NonceTooLow.is_retryable());
        assert!(!TxManagerError::NonceTooHigh.is_retryable());
        assert!(!TxManagerError::InsufficientFunds.is_retryable());
        assert!(!TxManagerError::IntrinsicGasTooLow.is_retryable());
        assert!(!TxManagerError::ExecutionReverted.is_retryable());
        assert!(!TxManagerError::MempoolDeadlineExpired.is_retryable());
        assert!(!TxManagerError::AlreadyReserved.is_retryable());
    }

    #[test]
    fn is_retryable_fee_errors() {
        assert!(TxManagerError::Underpriced.is_retryable());
        assert!(TxManagerError::ReplacementUnderpriced.is_retryable());
        assert!(TxManagerError::FeeTooLow.is_retryable());
        assert!(TxManagerError::MaxFeePerGasTooLow.is_retryable());
    }

    #[test]
    fn is_retryable_infra_errors() {
        assert!(TxManagerError::AlreadyKnown.is_retryable());
        assert!(TxManagerError::Rpc("any error".to_string()).is_retryable());
    }

    // ── is_already_known ─────────────────────────────────────────────────

    #[test]
    fn is_already_known_true() {
        assert!(TxManagerError::AlreadyKnown.is_already_known());
    }

    #[test]
    fn is_already_known_false_for_other_variants() {
        assert!(!TxManagerError::NonceTooLow.is_already_known());
        assert!(!TxManagerError::Underpriced.is_already_known());
        assert!(!TxManagerError::Rpc("already known".to_string()).is_already_known());
    }

    // ── Display output ────────────────────────────────────────────────────

    #[test]
    fn display_output_all_variants() {
        assert_eq!(TxManagerError::NonceTooLow.to_string(), "nonce too low");
        assert_eq!(TxManagerError::NonceTooHigh.to_string(), "nonce too high");
        assert_eq!(TxManagerError::InsufficientFunds.to_string(), "insufficient funds");
        assert_eq!(TxManagerError::IntrinsicGasTooLow.to_string(), "intrinsic gas too low");
        assert_eq!(TxManagerError::ExecutionReverted.to_string(), "execution reverted");
        assert_eq!(TxManagerError::MempoolDeadlineExpired.to_string(), "mempool deadline expired");
        assert_eq!(TxManagerError::AlreadyReserved.to_string(), "nonce already reserved");
        assert_eq!(TxManagerError::Underpriced.to_string(), "transaction underpriced");
        assert_eq!(
            TxManagerError::ReplacementUnderpriced.to_string(),
            "replacement transaction underpriced"
        );
        assert_eq!(TxManagerError::FeeTooLow.to_string(), "fee too low");
        assert_eq!(
            TxManagerError::MaxFeePerGasTooLow.to_string(),
            "max fee per gas less than block base fee"
        );
        assert_eq!(TxManagerError::AlreadyKnown.to_string(), "transaction already known");
        assert_eq!(TxManagerError::Rpc("test".to_string()).to_string(), "rpc error: test");
    }

    // ── Edge cases ──────────────────────────────────────────────────────

    #[test]
    fn classify_empty_string_returns_rpc_fallback() {
        let err = RpcErrorClassifier::classify_rpc_error("");
        assert!(matches!(err, TxManagerError::Rpc(ref s) if s.is_empty()));
    }

    // ── Negative classifier tests: internal-only variants ───────────────

    #[test]
    fn classify_mempool_deadline_not_recognized() {
        let err = RpcErrorClassifier::classify_rpc_error("mempool deadline expired");
        assert!(
            matches!(err, TxManagerError::Rpc(_)),
            "MempoolDeadlineExpired is internal-only, classifier must not produce it"
        );
    }

    #[test]
    fn classify_already_reserved_not_recognized() {
        let err = RpcErrorClassifier::classify_rpc_error("nonce already reserved");
        assert!(
            matches!(err, TxManagerError::Rpc(_)),
            "AlreadyReserved is internal-only, classifier must not produce it"
        );
    }
}
