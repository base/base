//! Error types for the flashblocks state processor.

use alloy_consensus::crypto::RecoveryError;
use alloy_primitives::{Address, B256};
use thiserror::Error;

/// Errors that can occur during flashblock state processing.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum StateProcessorError {
    // ==================== Protocol Errors (Invalid Sequences) ====================
    /// Invalid flashblock sequence or ordering.
    #[error("invalid flashblock sequence: flashblocks must be processed in order")]
    InvalidFlashblockSequence,

    /// First flashblock in a sequence must contain a base payload.
    #[error("missing base: first flashblock in sequence must contain a base payload")]
    MissingBase,

    /// Cannot build from an empty flashblocks collection.
    #[error("empty flashblocks: cannot build state from zero flashblocks")]
    EmptyFlashblocks,

    // ==================== Operational Errors (Infrastructure) ====================
    /// Missing canonical header for a given block number.
    #[error("missing canonical header for block {block_number}")]
    MissingCanonicalHeader {
        /// The block number for which the header is missing.
        block_number: u64,
    },

    /// State provider error with context.
    #[error("state provider error: {0}")]
    StateProvider(String),

    // ==================== Execution Errors (Transaction Processing) ====================
    /// Transaction execution failed.
    #[error("transaction execution failed for tx {tx_hash} from sender {sender}: {reason}")]
    TransactionExecution {
        /// The hash of the failed transaction.
        tx_hash: B256,
        /// The sender address of the failed transaction.
        sender: Address,
        /// The reason for the execution failure.
        reason: String,
    },

    /// ECDSA signature recovery failed.
    #[error("sender recovery failed: {0}")]
    SenderRecovery(String),

    /// Deposit transaction paired with a non-deposit receipt.
    #[error("deposit receipt mismatch: deposit transaction must have a deposit receipt")]
    DepositReceiptMismatch,

    /// Cumulative gas used overflow.
    #[error("gas overflow: cumulative gas used exceeded u64::MAX")]
    GasOverflow,

    /// EVM environment setup error.
    #[error("EVM environment error: {0}")]
    EvmEnv(String),

    /// L1 block info extraction error.
    #[error("L1 block info extraction error: {0}")]
    L1BlockInfo(String),

    /// Payload to block conversion error.
    #[error("block conversion error: {0}")]
    BlockConversion(String),

    /// Failed to load cache account for depositor.
    #[error("failed to load cache account for deposit transaction sender")]
    DepositAccountLoad,

    // ==================== Build Errors (Pending Blocks Construction) ====================
    /// Cannot build pending blocks without headers.
    #[error("missing headers: cannot build pending blocks without header information")]
    MissingHeaders,

    /// Cannot build pending blocks with no flashblocks.
    #[error("no flashblocks: cannot build pending blocks from empty flashblock collection")]
    NoFlashblocks,
}

impl From<RecoveryError> for StateProcessorError {
    fn from(err: RecoveryError) -> Self {
        Self::SenderRecovery(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::invalid_flashblock_sequence(
        StateProcessorError::InvalidFlashblockSequence,
        "invalid flashblock sequence: flashblocks must be processed in order"
    )]
    #[case::missing_base(
        StateProcessorError::MissingBase,
        "missing base: first flashblock in sequence must contain a base payload"
    )]
    #[case::empty_flashblocks(
        StateProcessorError::EmptyFlashblocks,
        "empty flashblocks: cannot build state from zero flashblocks"
    )]
    #[case::missing_canonical_header(
        StateProcessorError::MissingCanonicalHeader { block_number: 12345 },
        "missing canonical header for block 12345"
    )]
    #[case::state_provider(
        StateProcessorError::StateProvider("connection failed".to_string()),
        "state provider error: connection failed"
    )]
    #[case::deposit_receipt_mismatch(
        StateProcessorError::DepositReceiptMismatch,
        "deposit receipt mismatch: deposit transaction must have a deposit receipt"
    )]
    #[case::gas_overflow(
        StateProcessorError::GasOverflow,
        "gas overflow: cumulative gas used exceeded u64::MAX"
    )]
    #[case::evm_env(
        StateProcessorError::EvmEnv("invalid chain id".to_string()),
        "EVM environment error: invalid chain id"
    )]
    #[case::l1_block_info(
        StateProcessorError::L1BlockInfo("missing l1 data".to_string()),
        "L1 block info extraction error: missing l1 data"
    )]
    #[case::block_conversion(
        StateProcessorError::BlockConversion("invalid payload".to_string()),
        "block conversion error: invalid payload"
    )]
    #[case::deposit_account_load(
        StateProcessorError::DepositAccountLoad,
        "failed to load cache account for deposit transaction sender"
    )]
    #[case::missing_headers(
        StateProcessorError::MissingHeaders,
        "missing headers: cannot build pending blocks without header information"
    )]
    #[case::no_flashblocks(
        StateProcessorError::NoFlashblocks,
        "no flashblocks: cannot build pending blocks from empty flashblock collection"
    )]
    #[case::sender_recovery(
        StateProcessorError::SenderRecovery("invalid signature".to_string()),
        "sender recovery failed: invalid signature"
    )]
    fn test_error_display(#[case] error: StateProcessorError, #[case] expected: &str) {
        assert_eq!(error.to_string(), expected);
    }

    #[rstest]
    #[case::transaction_execution(
        StateProcessorError::TransactionExecution {
            tx_hash: B256::ZERO,
            sender: Address::ZERO,
            reason: "out of gas".to_string(),
        },
        &["transaction execution failed", "out of gas"]
    )]
    fn test_error_display_contains(#[case] error: StateProcessorError, #[case] substrings: &[&str]) {
        let display = error.to_string();
        for substring in substrings {
            assert!(display.contains(substring), "expected '{display}' to contain '{substring}'");
        }
    }

    #[rstest]
    #[case::missing_canonical_header(
        StateProcessorError::MissingCanonicalHeader { block_number: 100 }
    )]
    #[case::missing_base(StateProcessorError::MissingBase)]
    #[case::empty_flashblocks(StateProcessorError::EmptyFlashblocks)]
    #[case::missing_headers(StateProcessorError::MissingHeaders)]
    #[case::no_flashblocks(StateProcessorError::NoFlashblocks)]
    #[case::gas_overflow(StateProcessorError::GasOverflow)]
    fn test_error_debug(#[case] error: StateProcessorError) {
        let debug_str = format!("{:?}", error);
        assert!(!debug_str.is_empty());
    }

    #[test]
    fn test_error_in_result() {
        fn returns_ok() -> Result<u32, StateProcessorError> {
            Ok(42)
        }

        fn returns_err() -> Result<u32, StateProcessorError> {
            Err(StateProcessorError::GasOverflow)
        }

        assert!(returns_ok().is_ok());
        assert_eq!(returns_ok().unwrap(), 42);
        assert!(returns_err().is_err());
        assert!(matches!(returns_err().unwrap_err(), StateProcessorError::GasOverflow));
    }

    #[test]
    fn test_error_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}

        assert_send::<StateProcessorError>();
        assert_sync::<StateProcessorError>();
    }

    #[test]
    fn test_sender_recovery_from_impl() {
        let recovery_err = RecoveryError::new();
        let err: StateProcessorError = recovery_err.into();
        assert!(matches!(err, StateProcessorError::SenderRecovery(_)));
        assert!(err.to_string().contains("sender recovery failed"));
    }
}
