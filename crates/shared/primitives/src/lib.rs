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

/// Bundle types and test utilities for the TIPS transaction inclusion system.
pub mod bundles;
pub use bundles::{
    AcceptedBundle, Bundle, BundleExtensions, BundleHash, BundleTxs, CancelBundle,
    MeterBundleResponse, ParsedBundle, TransactionResult,
};

mod flashblocks;
pub use flashblocks::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, FlashblockDecodeError,
    FlashblocksPayloadV1, Metadata,
};

#[cfg(feature = "test-utils")]
pub mod test_utils;

// These dependencies are only used in test-utils feature
#[cfg(feature = "test-utils")]
pub use test_utils::*;
