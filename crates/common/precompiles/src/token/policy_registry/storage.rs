use alloy_primitives::{Address, address};
use base_precompile_macros::contract;

/// Singleton precompile address for the `PolicyRegistry`.
pub const POLICY_REGISTRY_ADDRESS: Address = address!("b030000000000000000000000000000000000000");

/// Storage layout for the `PolicyRegistry` precompile.
///
/// Slots are append-only — never reorder across hardforks.
#[contract(addr = POLICY_REGISTRY_ADDRESS)]
pub struct PolicyRegistryStorage {}
