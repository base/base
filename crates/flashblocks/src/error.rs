//! Error types for the flashblocks state processor.

use alloy_consensus::crypto::RecoveryError;
use alloy_primitives::{Address, B256};
use thiserror::Error;

/// Errors related to flashblock protocol sequencing and ordering.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum ProtocolError {
    /// Invalid flashblock sequence or ordering.
    #[error("invalid flashblock sequence: flashblocks must be processed in order")]
    InvalidSequence,

    /// First flashblock in a sequence must contain a base payload.
    #[error("missing base: first flashblock in sequence must contain a base payload")]
    MissingBase,

    /// Cannot build from an empty flashblocks collection.
    #[error("empty flashblocks: cannot build state from zero flashblocks")]
    EmptyFlashblocks,
}

/// Errors related to state provider and infrastructure operations.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum ProviderError {
    /// Missing canonical header for a given block number.
    #[error("missing canonical header for block {block_number}. This can be ignored if the node has recently restarted, restored from a snapshot or is still syncing.")]
    MissingCanonicalHeader {
        /// The block number for which the header is missing.
        block_number: u64,
    },

    /// State provider error with context.
    #[error("state provider error: {0}")]
    StateProvider(String),
}

/// Errors related to transaction execution and processing.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum ExecutionError {
    /// Transaction execution failed.
    #[error("transaction execution failed for tx {tx_hash} from sender {sender}: {reason}")]
    TransactionFailed {
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
}

impl From<RecoveryError> for ExecutionError {
    fn from(err: RecoveryError) -> Self {
        Self::SenderRecovery(err.to_string())
    }
}

/// Errors related to pending blocks construction.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum BuildError {
    /// Cannot build pending blocks without headers.
    #[error("missing headers: cannot build pending blocks without header information")]
    MissingHeaders,

    /// Cannot build pending blocks with no flashblocks.
    #[error("no flashblocks: cannot build pending blocks from empty flashblock collection")]
    NoFlashblocks,
}

/// Errors that can occur during flashblock state processing.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum StateProcessorError {
    /// Protocol-level errors (sequencing, ordering).
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    /// Provider/infrastructure errors.
    #[error(transparent)]
    Provider(#[from] ProviderError),

    /// Transaction execution errors.
    #[error(transparent)]
    Execution(#[from] ExecutionError),

    /// Pending blocks build errors.
    #[error(transparent)]
    Build(#[from] BuildError),
}

impl From<RecoveryError> for StateProcessorError {
    fn from(err: RecoveryError) -> Self {
        Self::Execution(ExecutionError::from(err))
    }
}

/// A type alias for `Result<T, StateProcessorError>`.
pub type Result<T> = std::result::Result<T, StateProcessorError>;

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::invalid_sequence(
        ProtocolError::InvalidSequence,
        "invalid flashblock sequence: flashblocks must be processed in order"
    )]
    #[case::missing_base(
        ProtocolError::MissingBase,
        "missing base: first flashblock in sequence must contain a base payload"
    )]
    #[case::empty_flashblocks(
        ProtocolError::EmptyFlashblocks,
        "empty flashblocks: cannot build state from zero flashblocks"
    )]
    fn test_protocol_error_display(#[case] error: ProtocolError, #[case] expected: &str) {
        assert_eq!(error.to_string(), expected);
    }

    #[rstest]
    #[case::missing_canonical_header(
        ProviderError::MissingCanonicalHeader { block_number: 12345 },
        "missing canonical header for block 12345. This can be ignored if the node has recently restarted, restored from a snapshot or is still syncing."
    )]
    #[case::state_provider(
        ProviderError::StateProvider("connection failed".to_string()),
        "state provider error: connection failed"
    )]
    fn test_provider_error_display(#[case] error: ProviderError, #[case] expected: &str) {
        assert_eq!(error.to_string(), expected);
    }

    #[rstest]
    #[case::deposit_receipt_mismatch(
        ExecutionError::DepositReceiptMismatch,
        "deposit receipt mismatch: deposit transaction must have a deposit receipt"
    )]
    #[case::gas_overflow(
        ExecutionError::GasOverflow,
        "gas overflow: cumulative gas used exceeded u64::MAX"
    )]
    #[case::evm_env(
        ExecutionError::EvmEnv("invalid chain id".to_string()),
        "EVM environment error: invalid chain id"
    )]
    #[case::l1_block_info(
        ExecutionError::L1BlockInfo("missing l1 data".to_string()),
        "L1 block info extraction error: missing l1 data"
    )]
    #[case::block_conversion(
        ExecutionError::BlockConversion("invalid payload".to_string()),
        "block conversion error: invalid payload"
    )]
    #[case::deposit_account_load(
        ExecutionError::DepositAccountLoad,
        "failed to load cache account for deposit transaction sender"
    )]
    #[case::sender_recovery(
        ExecutionError::SenderRecovery("invalid signature".to_string()),
        "sender recovery failed: invalid signature"
    )]
    fn test_execution_error_display(#[case] error: ExecutionError, #[case] expected: &str) {
        assert_eq!(error.to_string(), expected);
    }

    #[rstest]
    #[case::transaction_failed(
        ExecutionError::TransactionFailed {
            tx_hash: B256::ZERO,
            sender: Address::ZERO,
            reason: "out of gas".to_string(),
        },
        &["transaction execution failed", "out of gas"]
    )]
    fn test_execution_error_display_contains(
        #[case] error: ExecutionError,
        #[case] substrings: &[&str],
    ) {
        let display = error.to_string();
        for substring in substrings {
            assert!(display.contains(substring), "expected '{display}' to contain '{substring}'");
        }
    }

    #[rstest]
    #[case::missing_headers(
        BuildError::MissingHeaders,
        "missing headers: cannot build pending blocks without header information"
    )]
    #[case::no_flashblocks(
        BuildError::NoFlashblocks,
        "no flashblocks: cannot build pending blocks from empty flashblock collection"
    )]
    fn test_build_error_display(#[case] error: BuildError, #[case] expected: &str) {
        assert_eq!(error.to_string(), expected);
    }

    #[rstest]
    #[case::protocol(StateProcessorError::from(ProtocolError::InvalidSequence))]
    #[case::provider(StateProcessorError::from(ProviderError::MissingCanonicalHeader { block_number: 100 }))]
    #[case::execution(StateProcessorError::from(ExecutionError::GasOverflow))]
    #[case::build(StateProcessorError::from(BuildError::MissingHeaders))]
    fn test_state_processor_error_from_variants(#[case] error: StateProcessorError) {
        let debug_str = format!("{:?}", error);
        assert!(!debug_str.is_empty());
        let display_str = error.to_string();
        assert!(!display_str.is_empty());
    }

    #[test]
    fn test_error_in_result() {
        fn returns_ok() -> Result<u32, StateProcessorError> {
            Ok(42)
        }

        fn returns_err() -> Result<u32, StateProcessorError> {
            Err(ExecutionError::GasOverflow.into())
        }

        assert!(returns_ok().is_ok());
        assert_eq!(returns_ok().unwrap(), 42);
        assert!(returns_err().is_err());
        assert!(matches!(
            returns_err().unwrap_err(),
            StateProcessorError::Execution(ExecutionError::GasOverflow)
        ));
    }

    #[test]
    fn test_error_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}

        assert_send::<StateProcessorError>();
        assert_sync::<StateProcessorError>();
        assert_send::<ProtocolError>();
        assert_sync::<ProtocolError>();
        assert_send::<ProviderError>();
        assert_sync::<ProviderError>();
        assert_send::<ExecutionError>();
        assert_sync::<ExecutionError>();
        assert_send::<BuildError>();
        assert_sync::<BuildError>();
    }

    #[test]
    fn test_sender_recovery_from_impl() {
        let recovery_err = RecoveryError::new();
        let err: StateProcessorError = recovery_err.into();
        assert!(matches!(err, StateProcessorError::Execution(ExecutionError::SenderRecovery(_))));
        assert!(err.to_string().contains("sender recovery failed"));
    }

    #[test]
    fn test_error_category_matching() {
        let protocol_err: StateProcessorError = ProtocolError::InvalidSequence.into();
        assert!(matches!(protocol_err, StateProcessorError::Protocol(_)));

        let provider_err: StateProcessorError =
            ProviderError::MissingCanonicalHeader { block_number: 1 }.into();
        assert!(matches!(provider_err, StateProcessorError::Provider(_)));

        let execution_err: StateProcessorError = ExecutionError::GasOverflow.into();
        assert!(matches!(execution_err, StateProcessorError::Execution(_)));

        let build_err: StateProcessorError = BuildError::MissingHeaders.into();
        assert!(matches!(build_err, StateProcessorError::Build(_)));
    }
}
