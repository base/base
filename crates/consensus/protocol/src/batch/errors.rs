//! Span Batch Errors

/// Span Batch Errors
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum SpanBatchError {
    /// The span batch is too big
    #[error("The span batch is too big.")]
    TooBigSpanBatchSize,
    /// The bit field is too long
    #[error("The bit field is too long")]
    BitfieldTooLong,
    /// Empty Span Batch
    #[error("Empty span batch")]
    EmptySpanBatch,
    /// Future batch L1 origin before safe head
    #[error("Future batch L1 origin before safe head")]
    L1OriginBeforeSafeHead,
    /// Missing L1 origin
    #[error("Missing L1 origin")]
    MissingL1Origin,
    /// Decoding errors
    #[error("Span batch decoding error: {0}")]
    Decoding(#[from] SpanDecodingError),
}

/// An error encoding a batch.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum BatchEncodingError {
    /// Error encoding an Alloy RLP
    #[error("Error encoding an Alloy RLP: {0}")]
    AlloyRlpError(alloy_rlp::Error),
    /// Error encoding a span batch
    #[error("Error encoding a span batch: {0}")]
    SpanBatchError(#[from] SpanBatchError),
    /// Error encoding a native BLS chunk batch
    #[error("Error encoding a BLS chunk batch: {0}")]
    BlsChunkError(#[from] BlsChunkError),
}

/// An error decoding a batch.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum BatchDecodingError {
    /// Empty buffer
    #[error("Empty buffer")]
    EmptyBuffer,
    /// Error decoding an Alloy RLP
    #[error("Error decoding an Alloy RLP: {0}")]
    AlloyRlpError(alloy_rlp::Error),
    /// Error decoding a span batch
    #[error("Error decoding a span batch: {0}")]
    SpanBatchError(#[from] SpanBatchError),
    /// Invalid batch type
    #[error("Invalid batch type: {0}")]
    InvalidBatchType(u8),
    /// Error decoding a native BLS chunk batch
    #[error("Error decoding a BLS chunk batch: {0}")]
    BlsChunkError(#[from] BlsChunkError),
}

/// A structurally invalid native BLS chunk batch.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum BlsChunkError {
    /// The chunk has no members.
    #[error("BLS chunk has no members")]
    Empty,
    /// The chunk has more members than allowed.
    #[error("BLS chunk has {0} members, exceeding the maximum")]
    TooManyBlocks(usize),
    /// The chunk has more transfers than allowed.
    #[error("BLS chunk has {0} transfers, exceeding the maximum")]
    TooManyTransfers(usize),
    /// The chunk carries transfers but no aggregate signature.
    #[error("BLS chunk carries transfers without an aggregate")]
    MissingAggregate,
    /// The chunk carries an aggregate signature but no transfers.
    #[error("BLS chunk carries an aggregate without transfers")]
    UnexpectedAggregate,
    /// The aggregate field is neither empty nor 48 bytes.
    #[error("BLS chunk aggregate has invalid length {0}")]
    InvalidAggregateLength(usize),
    /// The encoding has bytes after the chunk.
    #[error("BLS chunk encoding has trailing bytes")]
    TrailingBytes,
    /// The RLP encoding is malformed.
    #[error("BLS chunk RLP error: {0}")]
    Rlp(#[from] alloy_rlp::Error),
}

/// Decoding Error
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum SpanDecodingError {
    /// Failed to decode relative timestamp
    #[error("Failed to decode relative timestamp")]
    RelativeTimestamp,
    /// Failed to decode L1 origin number
    #[error("Failed to decode L1 origin number")]
    L1OriginNumber,
    /// Failed to decode parent check
    #[error("Failed to decode parent check")]
    ParentCheck,
    /// Failed to decode L1 origin check
    #[error("Failed to decode L1 origin check")]
    L1OriginCheck,
    /// Failed to decode block count
    #[error("Failed to decode block count")]
    BlockCount,
    /// Failed to decode block tx counts
    #[error("Failed to decode block tx counts")]
    BlockTxCounts,
    /// Failed to decode transaction nonces
    #[error("Failed to decode transaction nonces")]
    TxNonces,
    /// Mismatch in length between the transaction type and signature arrays in a span batch
    /// transaction payload.
    #[error("Mismatch in length between the transaction type and signature arrays")]
    TypeSignatureLenMismatch,
    /// Invalid transaction type
    #[error("Invalid transaction type")]
    InvalidTransactionType,
    /// Invalid transaction data
    #[error("Invalid transaction data")]
    InvalidTransactionData,
    /// Invalid EIP-8130 authentication data
    #[error("Invalid EIP-8130 auth data")]
    InvalidAuthData,
    /// Invalid transaction signature
    #[error("Invalid transaction signature")]
    InvalidTransactionSignature,
}
