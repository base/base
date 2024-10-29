//! Contains batch types.

mod r#type;
pub use r#type::*;

mod core;
pub use core::Batch;

mod raw;
pub use raw::RawSpanBatch;

mod payload;
pub use payload::SpanBatchPayload;

mod prefix;
pub use prefix::SpanBatchPrefix;

mod inclusion;
pub use inclusion::BatchWithInclusionBlock;

mod errors;
pub use errors::{BatchDecodingError, SpanBatchError, SpanDecodingError};

mod bits;
pub use bits::SpanBatchBits;

mod span;
pub use span::SpanBatch;

mod transactions;
pub use transactions::SpanBatchTransactions;

mod element;
pub use element::{SpanBatchElement, MAX_SPAN_BATCH_ELEMENTS};

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
