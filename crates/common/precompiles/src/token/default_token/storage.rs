use alloy_primitives::{Address, address};
use base_precompile_macros::contract;

/// Canonical precompile address for the DefaultToken (placeholder — set actual address before deployment).
pub const DEFAULT_TOKEN_ADDRESS: Address = address!("0000000000000000000000000000000000000900");

#[contract(addr = DEFAULT_TOKEN_ADDRESS)]
pub struct DefaultToken {}
