#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

// Re-export from base-primitives for backwards compatibility
pub use base_primitives::{
    AccessListContract, Account, ContractFactory, DEVNET_CHAIN_ID, DoubleCounter, Logic, Logic2,
    Minimal7702Account, MockERC20, Proxy, SimpleStorage, TransparentUpgradeableProxy,
    build_test_genesis,
};

mod constants;
pub use constants::{
    BLOCK_BUILD_DELAY_MS, BLOCK_TIME_SECONDS, DEFAULT_JWT_SECRET, GAS_LIMIT,
    L1_BLOCK_INFO_DEPOSIT_TX, L1_BLOCK_INFO_DEPOSIT_TX_HASH, NODE_STARTUP_DELAY_MS, NamedChain,
};

mod engine;
pub use engine::{EngineAddress, EngineApi, EngineProtocol, HttpEngine, IpcEngine};

mod fixtures;
pub use fixtures::{create_provider_factory, load_chain_spec};

mod harness;
pub use harness::{TestHarness, TestHarnessBuilder};

mod node;
// Re-export BaseNodeExtension for extension authors
pub use base_client_primitives::BaseNodeExtension;
pub use node::{LocalNode, LocalNodeProvider};

mod tracing;
// Re-export signer traits for use in tests
pub use alloy_signer::SignerSync;
pub use tracing::init_silenced_tracing;
