//! Hash precompiles, it contains SHA-256 and RIPEMD-160 hash precompiles
//! More details in [`run_sha256`] and [`run_ripemd160`]

use super::calc_linear_cost;
use crate::{
    interpreter::GasTracker,
    precompiles::{PrecompileOutput, PrecompileResult},
};

/// Computes the SHA-256 hash of the input data
///
/// This function follows specifications defined in the following references:
/// - [Ethereum Yellow Paper](https://ethereum.github.io/yellowpaper/paper.pdf)
/// - [Solidity Documentation on Mathematical and Cryptographic Functions](https://docs.soliditylang.org/en/develop/units-and-global-variables.html#mathematical-and-cryptographic-functions)
/// - [ 0x02](https://etherscan.io/address/0000000000000000000000000000000000000002)
pub fn run_sha256(input: &[u8], gas: &mut GasTracker) -> PrecompileResult {
    let cost = calc_linear_cost(input.len(), 60, 12);
    gas.spend(cost)?;
    let output = crate::precompiles::crypto().sha256(input);
    Ok(PrecompileOutput::new(output.to_vec().into()))
}

/// Computes the RIPEMD-160 hash of the input data
///
/// This function follows specifications defined in the following references:
/// - [Ethereum Yellow Paper](https://ethereum.github.io/yellowpaper/paper.pdf)
/// - [Solidity Documentation on Mathematical and Cryptographic Functions](https://docs.soliditylang.org/en/develop/units-and-global-variables.html#mathematical-and-cryptographic-functions)
/// - [ 03](https://etherscan.io/address/0000000000000000000000000000000000000003)
pub fn run_ripemd160(input: &[u8], gas: &mut GasTracker) -> PrecompileResult {
    let gas_used = calc_linear_cost(input.len(), 600, 120);
    gas.spend(gas_used)?;
    let output = crate::precompiles::crypto().ripemd160(input);
    Ok(PrecompileOutput::new(output.to_vec().into()))
}
