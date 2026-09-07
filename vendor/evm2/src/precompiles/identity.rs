//! Identity precompile returns

use alloy_primitives::Bytes;

use super::calc_linear_cost;
use crate::{
    interpreter::GasTracker,
    precompiles::{PrecompileOutput, PrecompileResult},
};

/// The base cost of the operation
pub(crate) const IDENTITY_BASE: u64 = 15;
/// The cost per word
pub(crate) const IDENTITY_PER_WORD: u64 = 3;

/// Takes the input bytes, copies them, and returns it as the output.
///
/// See: <https://ethereum.github.io/yellowpaper/paper.pdf>
///
/// See: <https://etherscan.io/address/0000000000000000000000000000000000000004>
pub fn run(input: &[u8], gas: &mut GasTracker) -> PrecompileResult {
    let gas_used = calc_linear_cost(input.len(), IDENTITY_BASE, IDENTITY_PER_WORD);
    gas.spend(gas_used)?;
    Ok(PrecompileOutput::new(Bytes::copy_from_slice(input)))
}
