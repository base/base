//! Error types for conversions between Optimism Execution Payload Envelope
//! types and external types.

use alloy_eips::eip2718::Eip2718Error;
use alloy_primitives::B256;
use op_alloy_protocol::block_info::DecodeError;

/// An error that can occur when converting an [crate::OptimismExecutionPayloadEnvelopeV4] to an
/// [op_alloy_protocol::L2BlockInfo].
#[derive(Debug, derive_more::Display)]
pub enum ToL2BlockRefError {
    /// The genesis block hash does not match the expected value.
    #[display("Invalid genesis hash")]
    InvalidGenesisHash,
    /// The L2 block is missing the L1 info deposit transaction.
    #[display("L2 block is missing L1 info deposit transaction ({_0})")]
    MissingL1InfoDeposit(B256),
    /// The first payload transaction has an unexpected type.
    #[display("First payload transaction has unexpected type: {_0}")]
    UnexpectedTxType(u8),
    /// Failed to decode the first transaction into an [op_alloy_consensus::OpTxEnvelope].
    #[display("Failed to decode the first transaction into an OpTxEnvelope: {_0}")]
    TxEnvelopeDecodeError(Eip2718Error),
    /// The first payload transaction is not a deposit transaction.
    #[display("First payload transaction is not a deposit transaction, type: {_0}")]
    FirstTxNonDeposit(u8),
    /// Failed to decode the [op_alloy_protocol::L1BlockInfoTx] from the deposit transaction.
    #[display("Failed to decode the L1BlockInfoTx from the deposit transaction: {_0}")]
    BlockInfoDecodeError(DecodeError),
}

// Since `Eip2718Error` uses an msrv prior to rust `1.81`, the `core::error::Error` type
// is not stabalized and `Eip2718Error` only implements `std::error::Error` and not
// `core::error::Error`. So we need to implement `std::error::Error` to provide the `Eip2718Error`
// as a source when the `std` feature is enabled.
#[cfg(feature = "std")]
impl std::error::Error for ToL2BlockRefError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TxEnvelopeDecodeError(err) => Some(err),
            Self::BlockInfoDecodeError(err) => Some(err),
            _ => None,
        }
    }
}

#[cfg(not(feature = "std"))]
impl core::error::Error for ToL2BlockRefError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::BlockInfoDecodeError(err) => Some(err),
            _ => None,
        }
    }
}

/// An error that can occur when converting an [crate::OptimismExecutionPayloadEnvelopeV4] to a
/// [op_alloy_genesis::SystemConfig].
#[derive(Debug, derive_more::Display)]
pub enum ToSystemConfigError {
    /// The genesis block hash does not match the expected value.
    #[display("Invalid genesis hash")]
    InvalidGenesisHash,
    /// The L2 block is missing the L1 info deposit transaction.
    #[display("L2 block is missing L1 info deposit transaction ({_0})")]
    MissingL1InfoDeposit(B256),
    /// The first payload transaction has an unexpected type.
    #[display("First payload transaction has unexpected type: {_0}")]
    UnexpectedTxType(u8),
    /// Failed to decode the first transaction into an [op_alloy_consensus::OpTxEnvelope].
    #[display("Failed to decode the first transaction into an OpTxEnvelope: {_0}")]
    TxEnvelopeDecodeError(Eip2718Error),
    /// The first payload transaction is not a deposit transaction.
    #[display("First payload transaction is not a deposit transaction, type: {_0}")]
    FirstTxNonDeposit(u8),
    /// Failed to decode the [op_alloy_protocol::L1BlockInfoTx] from the deposit transaction.
    #[display("Failed to decode the L1BlockInfoTx from the deposit transaction: {_0}")]
    BlockInfoDecodeError(DecodeError),
    /// Missing system config in the genesis block.
    #[display("Missing system config in the genesis block")]
    MissingSystemConfig,
}

// Since `Eip2718Error` uses an msrv prior to rust `1.81`, the `core::error::Error` type
// is not stabalized and `Eip2718Error` only implements `std::error::Error` and not
// `core::error::Error`. So we need to implement `std::error::Error` to provide the `Eip2718Error`
// as a source when the `std` feature is enabled.
#[cfg(feature = "std")]
impl std::error::Error for ToSystemConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TxEnvelopeDecodeError(err) => Some(err),
            Self::BlockInfoDecodeError(err) => Some(err),
            _ => None,
        }
    }
}

#[cfg(not(feature = "std"))]
impl core::error::Error for ToSystemConfigError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::BlockInfoDecodeError(err) => Some(err),
            _ => None,
        }
    }
}
