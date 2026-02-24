//! Solidity contract bindings for integration tests.
//!
//! This module provides pre-compiled contract bindings that can be used
//! across different test crates without needing relative path references.

use alloy_sol_macro::sol;

sol!(
    #[sol(rpc)]
    DoubleCounter,
    concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/out/DoubleCounter.sol/DoubleCounter.json")
);

sol!(
    #[allow(clippy::too_many_arguments)]
    #[sol(rpc)]
    MockERC20,
    concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/out/MockERC20.sol/MockERC20.json")
);

sol!(
    #[sol(rpc)]
    TransparentUpgradeableProxy,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/contracts/out/TransparentUpgradeableProxy.sol/TransparentUpgradeableProxy.json"
    )
);

sol!(
    #[sol(rpc)]
    Minimal7702Account,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/contracts/out/Minimal7702Account.sol/Minimal7702Account.json"
    )
);

sol!(
    #[sol(rpc)]
    AccessListContract,
    concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/out/AccessList.sol/AccessList.json")
);

sol!(
    #[sol(rpc)]
    ContractFactory,
    concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/out/ContractFactory.sol/ContractFactory.json")
);

sol!(
    #[sol(rpc)]
    SimpleStorage,
    concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/out/ContractFactory.sol/SimpleStorage.json")
);

sol!(
    #[sol(rpc)]
    Proxy,
    concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/out/Proxy.sol/Proxy.json")
);

sol!(
    #[sol(rpc)]
    Logic,
    concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/out/Proxy.sol/Logic.json")
);

sol!(
    #[sol(rpc)]
    Logic2,
    concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/out/Proxy.sol/Logic2.json")
);
