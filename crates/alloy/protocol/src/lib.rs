#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/alloy.jpg",
    html_favicon_url = "https://raw.githubusercontent.com/alloy-rs/core/main/assets/favicon.ico"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(any(test, feature = "std")), no_std)]

extern crate alloc;

mod messages;
pub use messages::{ExecutingMessage, MessageIdentifier, MessagePayload, SafetyLevel};

mod batch;
pub use batch::{
    Batch, BatchDecodingError, BatchEncodingError, BatchReader, BatchTransaction, BatchType,
    BatchValidationProvider, BatchValidity, BatchWithInclusionBlock, RawSpanBatch, SingleBatch,
    SpanBatch, SpanBatchBits, SpanBatchEip1559TransactionData, SpanBatchEip2930TransactionData,
    SpanBatchElement, SpanBatchError, SpanBatchLegacyTransactionData, SpanBatchPayload,
    SpanBatchPrefix, SpanBatchTransactionData, SpanBatchTransactions, SpanDecodingError,
    MAX_SPAN_BATCH_ELEMENTS, SINGLE_BATCH_TYPE, SPAN_BATCH_TYPE,
};

mod errors;
pub use errors::OpBlockConversionError;

mod block;
pub use block::{BlockInfo, FromBlockError, L2BlockInfo};

mod frame;
pub use frame::{
    Frame, FrameDecodingError, FrameParseError, DERIVATION_VERSION_0, FRAME_OVERHEAD, MAX_FRAME_LEN,
};

mod compression;
pub use compression::{
    compress_zlib, decompress_brotli, decompress_zlib, BrotliDecompressionError, BrotliLevel,
    ChannelCompressor, CompressionAlgo, CompressorError, CompressorResult, CompressorType,
    CompressorWriter, Config, ZlibCompressor,
};
#[cfg(feature = "std")]
pub use compression::{
    BrotliCompressionError, BrotliCompressor, RatioCompressor, ShadowCompressor, VariantCompressor,
};

mod iter;
pub use iter::FrameIter;

mod utils;
pub use utils::{read_tx_data, starts_with_2718_deposit, to_system_config};

mod channel_out;
pub use channel_out::{ChannelOut, ChannelOutError};

mod channel;
pub use channel::{
    Channel, ChannelError, ChannelId, CHANNEL_ID_LENGTH, FJORD_MAX_RLP_BYTES_PER_CHANNEL,
    MAX_RLP_BYTES_PER_CHANNEL,
};

mod deposits;
pub use deposits::{
    decode_deposit, DepositError, DepositSourceDomain, DepositSourceDomainIdentifier,
    L1InfoDepositSource, UpgradeDepositSource, UserDepositSource, DEPOSIT_EVENT_ABI,
    DEPOSIT_EVENT_ABI_HASH, DEPOSIT_EVENT_VERSION_0,
};

mod info;
pub use info::{
    BlockInfoError, DecodeError, L1BlockInfoBedrock, L1BlockInfoEcotone, L1BlockInfoTx,
};

mod fee;
pub use fee::{
    calculate_tx_l1_cost_bedrock, calculate_tx_l1_cost_ecotone, calculate_tx_l1_cost_fjord,
    calculate_tx_l1_cost_regolith, data_gas_bedrock, data_gas_fjord, data_gas_regolith,
};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
