#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod action;
pub use action::{Action, L2BlockProvider};

mod conductor;
pub use conductor::{ConductorState, TestConductor, TestConductorHandle};

mod l1;
pub use l1::{
    ActionBlobProvider, ActionL1BlockFetcher, ActionL1ChainProvider, ActionL1FetcherError, L1Block,
    L1Miner, L1MinerConfig, L1PendingTransaction, L1ProviderError, L1TxBuilder, ReorgError,
    SharedL1Chain, UserDeposit, block_info_from, l1_block_to_rpc,
};

mod l2;
pub use l2::{
    ActionL2Source, BlockHashInner, L2Sequencer, L2SequencerError, SharedBlockHashRegistry,
    TEST_ACCOUNT_ADDRESS, TEST_ACCOUNT_KEY, TestAccount,
};

mod harness;
pub use harness::ActionTestHarness;

mod batcher;
pub use batcher::{
    Batcher, BatcherConfig, BatcherError, Inner, L1MinerTxManager, L1SignedSubmission, Pending,
};

mod matrix;
pub use matrix::{ForkMatrix, ForkSetter};

mod test_rollup_config;
pub use test_rollup_config::TestRollupConfigBuilder;

mod providers;
pub use providers::{ActionL2ChainProvider, L2ProviderError};

mod p2p;
pub use p2p::{SupervisedP2P, TestGossipTransport, TestGossipTransportError};

mod engine;
pub use engine::{
    ActionEngineClient, ActionEngineClientInner, PendingPayload, TestBlockchainProvider,
    TestNodeTypes, TestPool, TestProviderFactory,
};

mod node;
pub use node::{
    ActionPipeline, DerivedBlock, NodeStepResult, ProductionDaProvider, TestRollupNode,
    VerifierError, VerifierPipeline,
};
