//! Contains batch types.

mod r#type;
pub use r#type::*;

mod errors;
pub use errors::{SpanBatchError, SpanDecodingError};

mod bits;
pub use bits::SpanBatchBits;

mod element;
pub use element::{SpanBatchElement, MAX_SPAN_BATCH_ELEMENTS};

mod transactions;
pub use transactions::SpanBatchTransactions;

mod validity;
pub use validity::BatchValidity;

mod single;
pub use single::SingleBatch;

mod tx_data;
pub use tx_data::{
    SpanBatchEip1559TransactionData, SpanBatchEip2930TransactionData,
    SpanBatchLegacyTransactionData, SpanBatchTransactionData,
};

mod traits;
pub use traits::BatchValidationProvider;
