#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[cfg(feature = "engine")]
pub mod engine;
#[cfg(feature = "engine")]
pub use engine::{
    BlockProvider, DirectEngineApi, EIP1186AccountProofResponse, EngineApiClient, EngineApiError,
    EngineApiResult, LegacyPayloadSupport, ProofProvider, RollupConfigProvider, StorageKey,
};

mod bundles;
pub use bundles::{
    AcceptedBundle, Bundle, BundleHash, CancelBundle, MeterBundleResponse, ParsedBundle,
    TransactionResult,
};
mod flashblocks;
pub use flashblocks::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, FlashblockDecodeError,
    FlashblocksPayloadV1, Metadata,
};

mod validated_tx;
pub use validated_tx::{
    ConversionError, InsertResult, TransactionSignature, ValidatedTransaction, base64_bytes,
};

#[cfg(feature = "test-utils")]
pub mod test_utils;
#[cfg(feature = "test-utils")]
pub use test_utils::*;
