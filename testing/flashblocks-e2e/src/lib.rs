#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

//! End-to-end testing library for node-reth flashblocks RPC.

mod client;
pub use client::TestClient;

pub mod harness;
pub use harness::{FlashblockHarness, FlashblocksStream, WebSocketSubscription};

mod runner;
pub use runner::{
    TestResult, TestSummary, list_tests, print_results_json, print_results_text, run_tests,
};

pub mod types;
pub use types::{
    Bundle, ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock,
    FlashblockMetadata, MeterBundleResponse, OpBlock, TransactionResult,
};

pub mod tests;
pub use tests::{SkipFn, Test, TestCategory, TestFn, TestSuite, build_test_suite};
