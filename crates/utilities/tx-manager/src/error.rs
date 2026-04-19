//! Transaction manager error types.

use alloy_primitives::Bytes;
use alloy_transport::TransportError;
use thiserror::Error;

/// Maximum number of revert-data bytes rendered in [`RevertDisplay`].
/// Keeps log lines bounded without hiding the selector and first few args.
const REVERT_DATA_DISPLAY_LIMIT: usize = 128;

/// Display helper for the [`TxManagerError::ExecutionReverted`] variant.
///
/// Formats the optional reason and data into a human-readable suffix:
/// - `: <reason>` when a decoded reason is available.
/// - `: 0x<hex>` when only raw data is present (truncated to
///   [`REVERT_DATA_DISPLAY_LIMIT`] bytes).
/// - Empty string when neither is available.
#[derive(Debug)]
pub struct RevertDisplay<'a>(Option<&'a str>, Option<&'a Bytes>);

impl std::fmt::Display for RevertDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.0, self.1) {
            (Some(reason), _) => write!(f, ": {reason}"),
            (None, Some(data)) if !data.is_empty() => {
                let limit = data.len().min(REVERT_DATA_DISPLAY_LIMIT);
                f.write_str(": 0x")?;
                for byte in &data[..limit] {
                    write!(f, "{byte:02x}")?;
                }
                if data.len() > REVERT_DATA_DISPLAY_LIMIT {
                    f.write_str("\u{2026}")?; // '…'
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

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

    /// EVM execution reverted, optionally carrying the raw revert data and
    /// a human-readable reason decoded from standard Solidity revert
    /// signatures (`Error(string)` / `Panic(uint256)`).
    #[error("execution reverted{}", RevertDisplay(reason.as_deref(), data.as_ref()))]
    ExecutionReverted {
        /// Human-readable revert reason, if one could be decoded.
        reason: Option<String>,
        /// Raw revert data bytes returned by the EVM, if available.
        data: Option<Bytes>,
    },

    /// Mempool inclusion deadline expired.
    #[error("mempool deadline expired")]
    MempoolDeadlineExpired,

    /// Nonce slot was already reserved.
    #[error("nonce already reserved")]
    AlreadyReserved,

    /// Nonce arithmetic overflowed `u64::MAX`.
    #[error("nonce overflow")]
    NonceOverflow,

    /// Nonce reservation failed due to repeated cache contention.
    #[error("nonce acquisition failed")]
    NonceAcquisitionFailed,

    /// Send response channel closed before a result was delivered.
    ///
    /// The background send task exited (panicked or was cancelled)
    /// before completing. Non-retryable.
    #[error("send response channel closed")]
    ChannelClosed,

    /// Calculated fee exceeds the configured fee-limit ceiling.
    ///
    /// Returned by [`crate::FeeCalculator::check_limits`] when the proposed fee
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

    /// Feature or transaction type is not supported.
    ///
    /// Returned when the manager encounters a request it cannot handle,
    /// such as blob (EIP-4844) transactions before they are implemented.
    /// Non-retryable because the unsupported condition is deterministic.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// Transaction signing failed.
    ///
    /// Wraps the underlying signer error. Non-retryable because signing
    /// failures are typically deterministic (wrong key, unsupported tx type).
    #[error("signing failed: {0}")]
    Sign(String),

    /// The outer send timeout elapsed before the transaction was confirmed.
    ///
    /// Non-retryable because the caller's deadline has already been exceeded.
    #[error("send timed out")]
    SendTimeout,

    /// Configuration is invalid.
    ///
    /// Returned when config validation fails or a chain ID mismatch is
    /// detected during construction. Non-retryable because configuration
    /// errors require operator intervention.
    #[error("invalid config: {0}")]
    InvalidConfig(String),

    /// Wallet construction failed.
    ///
    /// Returned when the [`SignerConfig`](crate::SignerConfig) cannot build
    /// an [`EthereumWallet`](alloy_network::EthereumWallet) — e.g. an
    /// invalid private key or unreachable remote signer endpoint.
    /// Non-retryable because the configuration is deterministically wrong.
    #[error("wallet construction failed: {0}")]
    WalletConstruction(String),

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

    /// Returns `true` only for [`TxManagerError::Rpc`].
    ///
    /// Used to gate RPC error metric recording so that recognised state
    /// errors (e.g. `NonceTooLow`, `ExecutionReverted`) do not inflate the
    /// RPC error counter.
    #[must_use]
    pub const fn is_rpc_error(&self) -> bool {
        matches!(self, Self::Rpc(_))
    }
}

/// Result type alias for transaction manager operations.
pub type TxManagerResult<T> = Result<T, TxManagerError>;

/// Classifies alloy [`TransportError`]s into structured [`TxManagerError`]
/// variants.
///
/// This mirrors the Go `txmgr` `errStringMatch` approach, enabling
/// the send loop to make retry/abort decisions based on error type.
///
/// For server-returned errors ([`RpcError::ErrorResp`]), classification uses
/// the structured [`ErrorPayload`] directly — matching on `payload.message`
/// for known geth substrings and extracting revert data via
/// [`ErrorPayload::as_revert_data`] + [`alloy_sol_types::decode_revert_reason`].
///
/// Other `RpcError` variants (transport failures, serialisation errors, etc.)
/// fall through to [`TxManagerError::Rpc`].
///
/// # Limitations
///
/// Substring matching relies on known geth error messages. Other Ethereum
/// clients (Erigon, Besu, Nethermind) may use different wording for
/// equivalent errors, causing them to fall through to the
/// [`TxManagerError::Rpc`] fallback.
///
/// [`RpcError::ErrorResp`]: alloy_json_rpc::RpcError::ErrorResp
/// [`ErrorPayload`]: alloy_json_rpc::ErrorPayload
/// [`ErrorPayload::as_revert_data`]: alloy_json_rpc::ErrorPayload::as_revert_data
#[derive(Debug)]
pub struct RpcErrorClassifier;

impl RpcErrorClassifier {
    /// Classifies a [`TransportError`] into a [`TxManagerError`] variant.
    ///
    /// For server error responses the `payload.message` field is lowercased
    /// once and checked against known geth error substrings in a fixed
    /// order. The first match wins.
    ///
    /// **Ordering is critical**: `"replacement transaction underpriced"` is
    /// matched before `"transaction underpriced"` because the latter is a
    /// substring of the former.
    ///
    /// For `"execution reverted"` errors, structured revert data is
    /// extracted from the [`ErrorPayload`] via
    /// [`as_revert_data`](alloy_json_rpc::ErrorPayload::as_revert_data)
    /// and decoded with [`alloy_sol_types::decode_revert_reason`].
    ///
    /// Non-server errors and unrecognised messages fall through to
    /// [`TxManagerError::Rpc`], preserving the original error string.
    #[must_use]
    pub fn classify_rpc_error(error: &TransportError) -> TxManagerError {
        let Some(payload) = error.as_error_resp() else {
            return TxManagerError::Rpc(error.to_string());
        };

        let lowered = payload.message.to_ascii_lowercase();

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
        if let Some(pos) = lowered.find("execution reverted") {
            if let Some(data) = payload.as_revert_data() {
                let reason = alloy_sol_types::decode_revert_reason(&data);
                return TxManagerError::ExecutionReverted { reason, data: Some(data) };
            }
            // `to_ascii_lowercase()` is byte-offset-preserving, so
            // offsets from `lowered` are safe to index `payload.message`.
            let after = &payload.message[pos + "execution reverted".len()..];
            let after = after.trim_start_matches(':').trim();
            let reason = if after.is_empty() { None } else { Some(after.to_string()) };
            return TxManagerError::ExecutionReverted { reason, data: None };
        }
        if lowered.contains("fee too low") {
            return TxManagerError::FeeTooLow;
        }
        if lowered.contains("max fee per gas less than block base fee") {
            return TxManagerError::MaxFeePerGasTooLow;
        }
        if lowered.contains("already known") || lowered.contains("transaction already in pool") {
            return TxManagerError::AlreadyKnown;
        }

        TxManagerError::Rpc(payload.message.to_string())
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
    use std::borrow::Cow;

    use alloy_json_rpc::{ErrorPayload, RpcError};
    use alloy_transport::TransportErrorKind;
    use rstest::rstest;

    use super::*;

    /// Helper: build a [`TransportError`] with the given message.
    fn error_resp(msg: &str) -> TransportError {
        RpcError::ErrorResp(ErrorPayload {
            code: -32000,
            message: Cow::Owned(msg.to_string()),
            data: None,
        })
    }

    /// Helper: build a [`TransportError`] with message and raw JSON data.
    fn error_resp_with_data(msg: &str, data_json: &str) -> TransportError {
        use serde_json::value::RawValue;
        let raw: Box<RawValue> = serde_json::from_str(data_json).unwrap();
        RpcError::ErrorResp(ErrorPayload {
            code: 3,
            message: Cow::Owned(msg.to_string()),
            data: Some(raw),
        })
    }

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
    #[case::execution_reverted("execution reverted", TxManagerError::ExecutionReverted { reason: None, data: None })]
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
        let transport_err = error_resp(input);
        assert_eq!(RpcErrorClassifier::classify_rpc_error(&transport_err), expected);
    }

    #[test]
    fn classify_non_error_resp_falls_through_to_rpc() {
        let err: TransportError = RpcError::Transport(TransportErrorKind::BackendGone);
        let classified = RpcErrorClassifier::classify_rpc_error(&err);
        assert!(matches!(classified, TxManagerError::Rpc(_)));
    }

    // ── is_retryable ────────────────────────────────────────────────────

    #[rstest]
    #[case::nonce_too_low(TxManagerError::NonceTooLow, false)]
    #[case::nonce_too_high(TxManagerError::NonceTooHigh, false)]
    #[case::insufficient_funds(TxManagerError::InsufficientFunds, false)]
    #[case::intrinsic_gas_too_low(TxManagerError::IntrinsicGasTooLow, false)]
    #[case::execution_reverted(TxManagerError::ExecutionReverted { reason: None, data: None }, false)]
    #[case::mempool_deadline(TxManagerError::MempoolDeadlineExpired, false)]
    #[case::already_reserved(TxManagerError::AlreadyReserved, false)]
    #[case::channel_closed(TxManagerError::ChannelClosed, false)]
    #[case::fee_limit_exceeded(TxManagerError::FeeLimitExceeded { fee: 0, ceiling: 0 }, false)]
    #[case::invalid_safe_abort(TxManagerError::InvalidSafeAbortNonceTooLowCount, false)]
    #[case::nonce_overflow(TxManagerError::NonceOverflow, false)]
    #[case::nonce_acquisition_failed(TxManagerError::NonceAcquisitionFailed, false)]
    #[case::unsupported(TxManagerError::Unsupported("test".to_string()), false)]
    #[case::sign(TxManagerError::Sign("test".to_string()), false)]
    #[case::invalid_config(TxManagerError::InvalidConfig("test".to_string()), false)]
    #[case::wallet_construction(TxManagerError::WalletConstruction("test".to_string()), false)]
    #[case::send_timeout(TxManagerError::SendTimeout, false)]
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

    // ── is_rpc_error ─────────────────────────────────────────────────────

    #[rstest]
    #[case::rpc(TxManagerError::Rpc("any error".to_string()), true)]
    #[case::nonce_too_low(TxManagerError::NonceTooLow, false)]
    #[case::underpriced(TxManagerError::Underpriced, false)]
    #[case::already_known(TxManagerError::AlreadyKnown, false)]
    #[case::channel_closed(TxManagerError::ChannelClosed, false)]
    fn is_rpc_error(#[case] error: TxManagerError, #[case] expected: bool) {
        assert_eq!(error.is_rpc_error(), expected);
    }

    // ── Display output ──────────────────────────────────────────────────

    #[rstest]
    #[case::nonce_too_low(TxManagerError::NonceTooLow, "nonce too low")]
    #[case::nonce_too_high(TxManagerError::NonceTooHigh, "nonce too high")]
    #[case::insufficient_funds(TxManagerError::InsufficientFunds, "insufficient funds")]
    #[case::intrinsic_gas_too_low(TxManagerError::IntrinsicGasTooLow, "intrinsic gas too low")]
    #[case::execution_reverted(TxManagerError::ExecutionReverted { reason: None, data: None }, "execution reverted")]
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
    #[case::nonce_acquisition_failed(
        TxManagerError::NonceAcquisitionFailed,
        "nonce acquisition failed"
    )]
    #[case::send_timeout(TxManagerError::SendTimeout, "send timed out")]
    #[case::rpc(TxManagerError::Rpc("test".to_string()), "rpc error: test")]
    #[case::unsupported(TxManagerError::Unsupported("blob tx".to_string()), "unsupported: blob tx")]
    #[case::sign(TxManagerError::Sign("key error".to_string()), "signing failed: key error")]
    #[case::invalid_config(
        TxManagerError::InvalidConfig("bad value".to_string()),
        "invalid config: bad value"
    )]
    #[case::wallet_construction(
        TxManagerError::WalletConstruction("bad key".to_string()),
        "wallet construction failed: bad key"
    )]
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

    // ── revert data extraction via ErrorPayload ─────────────────────────

    #[test]
    fn classify_execution_reverted_with_structured_data() {
        // Error("out of tokens") encoded as ABI data in the ErrorPayload's
        // JSON data field — extracted via ErrorPayload::as_revert_data()
        // and decoded via alloy_sol_types::decode_revert_reason().
        let hex_data = "0x08c379a0\
             0000000000000000000000000000000000000000000000000000000000000020\
             000000000000000000000000000000000000000000000000000000000000000d\
             6f7574206f6620746f6b656e7300000000000000000000000000000000000000";
        let err = error_resp_with_data("execution reverted: ", &format!("\"{hex_data}\""));
        match RpcErrorClassifier::classify_rpc_error(&err) {
            TxManagerError::ExecutionReverted { reason, data } => {
                // alloy_sol_types::decode_revert_reason uses
                // RevertReason::to_string() which prefixes Error(string)
                // reverts with "revert: ".
                assert_eq!(reason.as_deref(), Some("revert: out of tokens"));
                assert!(data.is_some());
            }
            other => panic!("expected ExecutionReverted, got {other:?}"),
        }
    }

    #[test]
    fn classify_execution_reverted_plain_text() {
        let err = error_resp("execution reverted: GameAlreadyExists()");
        match RpcErrorClassifier::classify_rpc_error(&err) {
            TxManagerError::ExecutionReverted { reason, data } => {
                assert_eq!(reason.as_deref(), Some("GameAlreadyExists()"));
                assert!(data.is_none());
            }
            other => panic!("expected ExecutionReverted, got {other:?}"),
        }
    }

    #[test]
    fn revert_display_with_reason() {
        let err = TxManagerError::ExecutionReverted {
            reason: Some("not enough gas".to_string()),
            data: None,
        };
        assert_eq!(err.to_string(), "execution reverted: not enough gas");
    }

    #[test]
    fn revert_display_with_data_only() {
        let err = TxManagerError::ExecutionReverted {
            reason: None,
            data: Some(Bytes::from(vec![0xde, 0xad, 0xbe, 0xef])),
        };
        assert_eq!(err.to_string(), "execution reverted: 0xdeadbeef");
    }

    #[test]
    fn revert_display_empty() {
        let err = TxManagerError::ExecutionReverted { reason: None, data: None };
        assert_eq!(err.to_string(), "execution reverted");
    }
}
