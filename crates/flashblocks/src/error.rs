//! Error types for the flashblocks state processor.

use alloy_consensus::crypto::RecoveryError;
use alloy_primitives::{Address, B256};
use thiserror::Error;

/// A type alias for `Result<T, StateProcessorError>`.
pub type Result<T> = std::result::Result<T, StateProcessorError>;

/// Errors that can occur during flashblock state processing.
#[derive(Debug, Error)]
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
    SenderRecovery(#[from] RecoveryError),

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display_protocol_errors() {
        // Test InvalidFlashblockSequence
        let err = StateProcessorError::InvalidFlashblockSequence;
        assert_eq!(
            err.to_string(),
            "invalid flashblock sequence: flashblocks must be processed in order"
        );

        // Test MissingBase
        let err = StateProcessorError::MissingBase;
        assert_eq!(
            err.to_string(),
            "missing base: first flashblock in sequence must contain a base payload"
        );

        // Test EmptyFlashblocks
        let err = StateProcessorError::EmptyFlashblocks;
        assert_eq!(err.to_string(), "empty flashblocks: cannot build state from zero flashblocks");
    }

    #[test]
    fn test_error_display_operational_errors() {
        // Test MissingCanonicalHeader
        let err = StateProcessorError::MissingCanonicalHeader { block_number: 12345 };
        assert_eq!(err.to_string(), "missing canonical header for block 12345");

        // Test StateProvider
        let err = StateProcessorError::StateProvider("connection failed".to_string());
        assert_eq!(err.to_string(), "state provider error: connection failed");
    }

    #[test]
    fn test_error_display_execution_errors() {
        // Test TransactionExecution
        let tx_hash = B256::ZERO;
        let sender = Address::ZERO;
        let err = StateProcessorError::TransactionExecution {
            tx_hash,
            sender,
            reason: "out of gas".to_string(),
        };
        assert!(err.to_string().contains("transaction execution failed"));
        assert!(err.to_string().contains("out of gas"));

        // Test DepositReceiptMismatch
        let err = StateProcessorError::DepositReceiptMismatch;
        assert_eq!(
            err.to_string(),
            "deposit receipt mismatch: deposit transaction must have a deposit receipt"
        );

        // Test GasOverflow
        let err = StateProcessorError::GasOverflow;
        assert_eq!(err.to_string(), "gas overflow: cumulative gas used exceeded u64::MAX");

        // Test EvmEnv
        let err = StateProcessorError::EvmEnv("invalid chain id".to_string());
        assert_eq!(err.to_string(), "EVM environment error: invalid chain id");

        // Test L1BlockInfo
        let err = StateProcessorError::L1BlockInfo("missing l1 data".to_string());
        assert_eq!(err.to_string(), "L1 block info extraction error: missing l1 data");

        // Test BlockConversion
        let err = StateProcessorError::BlockConversion("invalid payload".to_string());
        assert_eq!(err.to_string(), "block conversion error: invalid payload");

        // Test DepositAccountLoad
        let err = StateProcessorError::DepositAccountLoad;
        assert_eq!(
            err.to_string(),
            "failed to load cache account for deposit transaction sender"
        );
    }

    #[test]
    fn test_error_display_build_errors() {
        // Test MissingHeaders
        let err = StateProcessorError::MissingHeaders;
        assert_eq!(
            err.to_string(),
            "missing headers: cannot build pending blocks without header information"
        );

        // Test NoFlashblocks
        let err = StateProcessorError::NoFlashblocks;
        assert_eq!(
            err.to_string(),
            "no flashblocks: cannot build pending blocks from empty flashblock collection"
        );
    }

    #[test]
    fn test_error_pattern_matching() {
        // Test that we can pattern match on specific error variants
        let err = StateProcessorError::MissingCanonicalHeader { block_number: 100 };
        assert!(matches!(err, StateProcessorError::MissingCanonicalHeader { block_number: 100 }));

        let err = StateProcessorError::MissingBase;
        assert!(matches!(err, StateProcessorError::MissingBase));

        let err = StateProcessorError::EmptyFlashblocks;
        assert!(matches!(err, StateProcessorError::EmptyFlashblocks));

        let err = StateProcessorError::MissingHeaders;
        assert!(matches!(err, StateProcessorError::MissingHeaders));

        let err = StateProcessorError::NoFlashblocks;
        assert!(matches!(err, StateProcessorError::NoFlashblocks));
    }

    #[test]
    fn test_error_debug_impl() {
        // Verify Debug is implemented
        let err = StateProcessorError::GasOverflow;
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("GasOverflow"));
    }

    #[test]
    fn test_result_type_alias() {
        // Test that the Result type alias works correctly
        fn returns_ok() -> Result<u32> {
            Ok(42)
        }

        fn returns_err() -> Result<u32> {
            Err(StateProcessorError::GasOverflow)
        }

        assert!(returns_ok().is_ok());
        assert_eq!(returns_ok().unwrap(), 42);
        assert!(returns_err().is_err());
        assert!(matches!(returns_err().unwrap_err(), StateProcessorError::GasOverflow));
    }

    #[test]
    fn test_error_is_send_sync() {
        // Verify the error type is Send + Sync for use in async contexts
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}

        assert_send::<StateProcessorError>();
        assert_sync::<StateProcessorError>();
    }

    #[test]
    fn test_sender_recovery_from_impl() {
        // Test that RecoveryError can be converted into StateProcessorError
        let recovery_err = RecoveryError::new();
        let err: StateProcessorError = recovery_err.into();
        assert!(matches!(err, StateProcessorError::SenderRecovery(_)));
        assert!(err.to_string().contains("sender recovery failed"));
    }
}
