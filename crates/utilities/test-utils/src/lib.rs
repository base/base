#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod accounts;
pub use accounts::Account;

mod genesis;
pub use genesis::{DEVNET_CHAIN_ID, GENESIS_GAS_LIMIT, build_test_genesis};

mod contracts;
pub use contracts::{
    AccessListContract, ContractFactory, DoubleCounter, Logic, Logic2, Minimal7702Account,
    MockERC20, Proxy, SimpleStorage, TransparentUpgradeableProxy,
};
