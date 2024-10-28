//! Span Batch Errors

/// Span Batch Errors
#[derive(Debug, derive_more::Display, Clone, PartialEq, Eq)]
pub enum SpanBatchError {
    /// The span batch is too big
    #[display("The span batch is too big.")]
    TooBigSpanBatchSize,
    /// The bit field is too long
    #[display("The bit field is too long")]
    BitfieldTooLong,
    /// Empty Span Batch
    #[display("Empty span batch")]
    EmptySpanBatch,
    /// Missing L1 origin
    #[display("Missing L1 origin")]
    MissingL1Origin,
    /// Decoding errors
    #[display("Span batch decoding error: {_0}")]
    Decoding(SpanDecodingError),
}

impl From<SpanDecodingError> for SpanBatchError {
    fn from(err: SpanDecodingError) -> Self {
        Self::Decoding(err)
    }
}

impl core::error::Error for SpanBatchError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Decoding(err) => Some(err),
            _ => None,
        }
    }
}

/// Decoding Error
#[derive(Debug, derive_more::Display, Clone, PartialEq, Eq)]
pub enum SpanDecodingError {
    /// Failed to decode relative timestamp
    #[display("Failed to decode relative timestamp")]
    RelativeTimestamp,
    /// Failed to decode L1 origin number
    #[display("Failed to decode L1 origin number")]
    L1OriginNumber,
    /// Failed to decode parent check
    #[display("Failed to decode parent check")]
    ParentCheck,
    /// Failed to decode L1 origin check
    #[display("Failed to decode L1 origin check")]
    L1OriginCheck,
    /// Failed to decode block count
    #[display("Failed to decode block count")]
    BlockCount,
    /// Failed to decode block tx counts
    #[display("Failed to decode block tx counts")]
    BlockTxCounts,
    /// Failed to decode transaction nonces
    #[display("Failed to decode transaction nonces")]
    TxNonces,
    /// Mismatch in length between the transaction type and signature arrays in a span batch
    /// transaction payload.
    #[display("Mismatch in length between the transaction type and signature arrays")]
    TypeSignatureLenMismatch,
    /// Invalid transaction type
    #[display("Invalid transaction type")]
    InvalidTransactionType,
    /// Invalid transaction data
    #[display("Invalid transaction data")]
    InvalidTransactionData,
    /// Invalid transaction signature
    #[display("Invalid transaction signature")]
    InvalidTransactionSignature,
}

impl core::error::Error for SpanDecodingError {}
