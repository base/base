//! Error types for op-enclave operations.

use alloy_primitives::B256;
use thiserror::Error;

/// Errors that can occur during configuration operations.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum ConfigError {
    /// Binary serialization produced unexpected size.
    #[error("binary serialization failed: expected {expected} bytes, got {actual}")]
    SerializationSize {
        /// Expected number of bytes.
        expected: usize,
        /// Actual number of bytes produced.
        actual: usize,
    },
}

/// Errors that can occur during cryptographic operations.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum CryptoError {
    /// Signature has invalid length.
    #[error("invalid signature length: expected 65 bytes, got {0}")]
    InvalidSignatureLength(usize),
}

/// Errors that can occur during stateless block execution.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum ExecutorError {
    /// Invalid receipts: computed root does not match header.
    #[error("invalid receipts: computed root does not match header")]
    InvalidReceipts,

    /// Invalid parent hash.
    #[error("invalid parent hash")]
    InvalidParentHash,

    /// L1 origin is too old (sequencer drift exceeded).
    #[error("l1 origin is too old (sequencer drift exceeded)")]
    L1OriginTooOld,

    /// Sequenced transactions cannot include deposit transactions.
    #[error("sequenced txs cannot include deposits")]
    SequencedTxCannotBeDeposit,

    /// Invalid transaction hash.
    #[error("invalid tx hash")]
    InvalidTxHash,

    /// Invalid L1 origin.
    #[error("invalid L1 origin")]
    InvalidL1Origin,

    /// Invalid state root.
    #[error("invalid state root: expected {expected}, computed {computed}")]
    InvalidStateRoot {
        /// Expected state root from header.
        expected: B256,
        /// Computed state root from execution.
        computed: B256,
    },

    /// Invalid receipt hash.
    #[error("invalid receipt hash: expected {expected}, computed {computed}")]
    InvalidReceiptHash {
        /// Expected receipt hash from header.
        expected: B256,
        /// Computed receipt hash from execution.
        computed: B256,
    },

    /// Invalid message account address.
    #[error("invalid message account address")]
    InvalidMessageAccountAddress,

    /// Failed to verify message account proof.
    #[error("failed to verify message account: {0}")]
    MessageAccountVerificationFailed(String),

    /// Witness transformation failed.
    #[error("witness transformation failed: {0}")]
    WitnessTransformFailed(String),

    /// Stateless execution failed.
    #[error("stateless execution failed: {0}")]
    ExecutionFailed(String),

    /// Failed to prepare payload attributes.
    #[error("failed to prepare payload attributes: {0}")]
    AttributesBuildFailed(String),

    /// Transaction decoding failed.
    #[error("failed to decode transaction: {0}")]
    TxDecodeFailed(String),

    /// Provider error.
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// Errors that can occur during provider operations (L1/L2 data fetching).
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum ProviderError {
    /// Block not found by hash.
    #[error("block not found: {0}")]
    BlockNotFound(B256),

    /// Receipt trie root mismatch.
    #[error("invalid receipt root: expected {expected}, computed {computed}")]
    InvalidReceiptRoot {
        /// Expected receipt root from header.
        expected: B256,
        /// Computed receipt root from receipts.
        computed: B256,
    },

    /// Missing L1 info deposit transaction in L2 block.
    #[error("L2 block is missing L1 info deposit tx, block hash: {0}")]
    MissingL1InfoDeposit(B256),

    /// First transaction is not a deposit type.
    #[error("first payload tx has unexpected tx type: {0}")]
    UnexpectedTxType(u8),

    /// Failed to parse L1 block info from deposit transaction.
    #[error("failed to parse L1 info deposit tx from L2 block: {0}")]
    L1InfoParseError(String),

    /// Genesis block hash mismatch.
    #[error(
        "expected L2 genesis hash to match L2 block at genesis block number {number}: {expected} <> {actual}"
    )]
    GenesisHashMismatch {
        /// Genesis block number.
        number: u64,
        /// Expected genesis hash from config.
        expected: B256,
        /// Actual hash of the block.
        actual: B256,
    },

    /// Fee scalar value exceeds maximum allowed value.
    #[error("fee scalar overflow: {field} value {value} exceeds u32::MAX")]
    FeeScalarOverflow {
        /// The field name that overflowed.
        field: &'static str,
        /// The actual value.
        value: alloy_primitives::U256,
    },

    /// Account proof verification failed.
    #[error("account proof verification failed: {0}")]
    AccountProofFailed(String),
}

/// Top-level error type for enclave operations.
#[derive(Debug, Clone, Eq, PartialEq, Error)]
pub enum EnclaveError {
    /// Configuration error.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Cryptographic error.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    /// Provider error.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// Executor error.
    #[error(transparent)]
    Executor(#[from] ExecutorError),
}

/// A specialized Result type for enclave operations.
pub type Result<T> = std::result::Result<T, EnclaveError>;
