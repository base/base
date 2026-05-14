use alloy_primitives::{Address, U256, address};
use base_precompile_macros::contract;

/// Canonical address of the Counter precompile.
pub const COUNTER_ADDRESS: Address = address!("0000000000000000000000000000000000000900");

// Slots are append-only — never reorder or reuse across hardforks.
#[contract(addr = COUNTER_ADDRESS)]
pub struct Counter {
    pub count: U256, // slot 0
}
