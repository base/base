//! Precompile looper contract for calling precompiles multiple times per transaction.

use alloy_primitives::{Bytes, hex};

/// Precompile looper contract.
///
/// Solidity source:
/// ```solidity
/// // SPDX-License-Identifier: MIT
/// pragma solidity ^0.8.0;
///
/// contract PrecompileLooper {
///     function loopCall(address precompile, bytes calldata data, uint256 iterations) external {
///         for (uint256 i = 0; i < iterations; i++) {
///             (bool success,) = precompile.staticcall(data);
///             require(success, "precompile call failed");
///         }
///     }
/// }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct PrecompileLooper;

impl PrecompileLooper {
    /// Compiled bytecode for the `PrecompileLooper` contract.
    ///
    /// Compiled with solc 0.8.28, optimizer enabled (200 runs).
    pub const BYTECODE: &'static [u8] = &hex!(
        "6080604052348015600e575f5ffd5b506101538061001c5f395ff3fe608060405234801561000f575f5ffd5b5060043610610029575f3560e01c80637b395fc21461002d575b5f5ffd5b610047600480360381019061004291906100c5565b610049565b005b5f5f90505b818110156100895760605f8686866040516100699190610165565b5f604051808303815f8787f1925050503d805f811461009e578060405191505b50505050808061009d90610197565b91505061004e565b50505050505050565b5f5ffd5b5f5ffd5b5f5ffd5b5f5ffd5b5f83601f8401126100b8576100b76100aa565b5b823590506100c8602082018561009e565b9150509250929050565b5f5f5f5f606085870312156100eb576100ea6100a6565b5b5f6100f9888289016100ae565b94505060208501359250604085013591506060850135905092959194509250565b5f81519050919050565b5f82825260208201905092915050565b828183375f83830152505050565b5f601f19601f830116905090505b919050565b5f6101618284610124565b905092915050565b5f819050919050565b5f6101818261016d565b91507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff82036101b1576101b0610178565b5b60018201905091905056fea26469706673582212205f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f64736f6c634300081c0033"
    );

    /// Returns the deployment bytecode.
    pub const fn deployment_bytecode() -> Bytes {
        Bytes::from_static(Self::BYTECODE)
    }
}
