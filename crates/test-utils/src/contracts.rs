//! Solidity contract bindings for integration tests.
//!
//! This module provides pre-compiled contract bindings that can be used
//! across different test crates without needing relative path references.
//!
//! Contract sources:
//! - `DoubleCounter`: Custom test contract (src/DoubleCounter.sol)
//! - `MockERC20`: Solmate's MockERC20 (lib/solmate)
//! - `TransparentUpgradeableProxy`: OpenZeppelin's TransparentUpgradeableProxy (lib/openzeppelin-contracts)

use alloy_sol_macro::sol;

sol!(
    #[sol(rpc)]
    DoubleCounter,
    concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/out/DoubleCounter.sol/DoubleCounter.json")
);

sol!(
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
    PrivateBalance,
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/contracts/out/PrivateBalance.sol/PrivateBalance.json"
    )
);
