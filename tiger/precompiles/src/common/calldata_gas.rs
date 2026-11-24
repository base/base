//! Calldata gas charged by native precompile dispatch.

use base_tiger_precompile_storage::{Result, StorageCtx};

/// Calldata gas the activation registry, B-20 tokens, factory, and policy registry charge
/// before dispatch.
///
/// Emulates the cost a Solidity predeploy would incur reading its calldata:
/// `G_copy` (3 gas/word) + `G_memory` (3 gas/word) = 6 gas/word. Part of the receipts and
/// gas-used commitment, so it must match base-reth exactly.
#[derive(Debug, Clone, Copy)]
pub struct CalldataGas;

impl CalldataGas {
    /// Gas per 32-byte calldata word.
    pub const WORD: u64 = 6;

    /// Returns the calldata gas for `calldata`.
    pub const fn cost(calldata: &[u8]) -> u64 {
        (calldata.len() as u64).div_ceil(32).saturating_mul(Self::WORD)
    }

    /// Deducts the calldata gas for `calldata` from `ctx`.
    pub fn deduct(ctx: StorageCtx<'_>, calldata: &[u8]) -> Result<()> {
        ctx.deduct_gas(Self::cost(calldata))
    }
}
