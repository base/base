#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod accounts;
pub use accounts::{ALICE, Account, BOB, CHARLIE, DEPLOYER, TestAccount, TestAccounts};

mod constants;
pub use constants::{
    BASE_CHAIN_ID, BLOCK_BUILD_DELAY_MS, BLOCK_TIME_SECONDS, DEFAULT_JWT_SECRET, GAS_LIMIT,
    L1_BLOCK_INFO_DEPOSIT_TX, L1_BLOCK_INFO_DEPOSIT_TX_HASH, NODE_STARTUP_DELAY_MS, NamedChain,
};

mod contracts;
pub use contracts::{DoubleCounter, MockERC20, TransparentUpgradeableProxy};

mod engine;
pub use engine::{EngineAddress, EngineApi, EngineProtocol, HttpEngine, IpcEngine};

mod fixtures;
pub use fixtures::{create_provider_factory, load_genesis};

mod flashblocks_harness;
pub use flashblocks_harness::FlashblocksHarness;

mod harness;
pub use harness::TestHarness;

mod node;
pub use node::{
    FlashblocksLocalNode, FlashblocksParts, LocalFlashblocksState, LocalNode, LocalNodeProvider,
    OpAddOns, OpBuilder, OpComponentsBuilder, OpTypes, default_launcher,
};

mod tracing;
pub use tracing::init_silenced_tracing;
