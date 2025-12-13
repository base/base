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
    #[sol(rpc)]
    TestERC20,
    concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/out/TestERC20.sol/TestERC20.json")
);

sol!(
    #[sol(rpc)]
    TransparentProxy,
    concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/out/TransparentProxy.sol/TransparentProxy.json")
);
