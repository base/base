#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod batch;
pub use batch::{
    Batch, BatchDecodingError, BatchDropReason, BatchEncodingError, BatchReader, BatchTransaction,
    BatchType, BatchValidationProvider, BatchValidity, BatchWithInclusionBlock, DecompressionError,
    MAX_SPAN_BATCH_ELEMENTS, RawSpanBatch, SINGLE_BATCH_TYPE, SPAN_BATCH_TYPE, SingleBatch,
    SpanBatch, SpanBatchBits, SpanBatchEip1559TransactionData, SpanBatchEip2930TransactionData,
    SpanBatchEip7702TransactionData, SpanBatchElement, SpanBatchError,
    SpanBatchLegacyTransactionData, SpanBatchPayload, SpanBatchPrefix, SpanBatchTransactionData,
    SpanBatchTransactions, SpanDecodingError,
};

mod brotli;
pub use brotli::{BrotliDecompressionError, decompress_brotli};

mod attributes;
pub use attributes::OpAttributesWithParent;

mod errors;
pub use errors::OpBlockConversionError;

mod block;
pub use block::{BlockInfo, FromBlockError, L2BlockInfo};

mod frame;
pub use frame::{
    DERIVATION_VERSION_0, FRAME_OVERHEAD, Frame, FrameDecodingError, FrameParseError, MAX_FRAME_LEN,
};

mod utils;
pub use utils::{read_tx_data, to_system_config};

mod channel;
pub use channel::{
    CHANNEL_ID_LENGTH, Channel, ChannelError, ChannelId, FJORD_MAX_RLP_BYTES_PER_CHANNEL,
    MAX_RLP_BYTES_PER_CHANNEL,
};

mod deposits;
pub use deposits::{
    DEPOSIT_EVENT_ABI, DEPOSIT_EVENT_ABI_HASH, DEPOSIT_EVENT_VERSION_0, DepositError,
    decode_deposit,
};

mod info;
pub use info::{
    BlockInfoError, DecodeError, L1BlockInfoBedrock, L1BlockInfoBedrockBaseFields,
    L1BlockInfoBedrockFields, L1BlockInfoBedrockOnlyFields, L1BlockInfoEcotone,
    L1BlockInfoEcotoneBaseFields, L1BlockInfoEcotoneFields, L1BlockInfoEcotoneOnlyFields,
    L1BlockInfoIsthmus, L1BlockInfoIsthmusBaseFields, L1BlockInfoIsthmusFields, L1BlockInfoJovian,
    L1BlockInfoJovianBaseFields, L1BlockInfoJovianFields, L1BlockInfoTx,
};

mod predeploys;
pub use predeploys::{Deployers, Predeploys, SystemAddresses};

mod output_root;
pub use output_root::OutputRoot;

mod sync;
pub use sync::SyncStatus;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
