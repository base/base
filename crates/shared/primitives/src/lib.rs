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

#[cfg(feature = "test-utils")]
pub mod test_utils;

// These dependencies are only used in test-utils feature
#[cfg(feature = "test-utils")]
pub use test_utils::*;
