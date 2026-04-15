#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod batch;
pub use batch::{
    Batch, BatchDecodingError, BatchDropReason, BatchEncodingError, BatchReader, BatchTransaction,
    BatchType, BatchValidationProvider, BatchValidity, BatchWithInclusionBlock, DecompressionError,
    RawSpanBatch, SingleBatch, SpanBatch, SpanBatchBits, SpanBatchEip1559TransactionData,
    SpanBatchEip2930TransactionData, SpanBatchEip7702TransactionData, SpanBatchElement,
    SpanBatchError, SpanBatchLegacyTransactionData, SpanBatchPayload, SpanBatchPrefix,
    SpanBatchTransactionData, SpanBatchTransactions, SpanDecodingError,
};

mod brotli;
pub use brotli::{Brotli, BrotliDecompressionError};

mod attributes;
pub use attributes::AttributesWithParent;

mod errors;
pub use errors::BaseBlockConversionError;

mod block;
pub use block::{BlockInfo, FromBlockError, L2BlockInfo};

mod frame;
pub use frame::{DERIVATION_VERSION_0, Frame, FrameDecodingError, FrameParseError};

mod utils;
pub use utils::{read_tx_data, to_system_config};

mod channel;
pub use channel::{Channel, ChannelError, ChannelId};

mod deposits;
pub use deposits::{DepositDecodeError, Deposits};

mod info;
pub use info::{
    BlockInfoError, DecodeError, L1BlockInfoBedrock, L1BlockInfoBedrockBase,
    L1BlockInfoBedrockBaseFields, L1BlockInfoBedrockFields, L1BlockInfoBedrockOnlyFields,
    L1BlockInfoEcotone, L1BlockInfoEcotoneBase, L1BlockInfoEcotoneBaseFields,
    L1BlockInfoEcotoneFields, L1BlockInfoEcotoneOnlyFields, L1BlockInfoIsthmus,
    L1BlockInfoIsthmusBaseFields, L1BlockInfoIsthmusFields, L1BlockInfoJovian,
    L1BlockInfoJovianBaseFields, L1BlockInfoJovianFields, L1BlockInfoTx,
};

mod output_root;
pub use output_root::OutputRoot;

mod sync;
pub use sync::SyncStatus;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
