#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod action;
pub use action::{Action, L2BlockProvider};

mod miner;
pub use miner::{L1Block, L1Miner, L1MinerConfig, PendingTx, ReorgError, block_info_from};

mod l2;
pub use l2::{
    ActionL2Source, L2Sequencer, L2SequencerError, TEST_ACCOUNT_ADDRESS, TEST_ACCOUNT_KEY,
};

mod harness;
pub use harness::ActionTestHarness;

mod batcher;
pub use batcher::{
    BatchType, Batcher, BatcherConfig, BatcherError, ChannelDriverConfig, GarbageKind,
};

mod providers;
pub use providers::{
    ActionBlobDataSource, ActionBlobProvider, ActionDataSource, ActionL1ChainProvider,
    ActionL2ChainProvider, L1ProviderError, L2ProviderError, SharedL1Chain,
};

mod verifier;
pub use verifier::{BlobVerifierPipeline, L2Verifier, VerifierError, VerifierPipeline};
pub use base_consensus_derive::StepResult;
