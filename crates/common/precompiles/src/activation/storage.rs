//! Storage layout and constants for the activation registry.

use alloy_primitives::{Address, B256, address, b256};
use base_precompile_macros::contract;
use base_precompile_storage::Mapping;

/// Activation registry precompile address.
pub const ACTIVATION_REGISTRY_ADDRESS: Address =
    address!("0x84530000000000000000000000000000000000ff");

/// Temporary activation admin address.
///
/// Replace this with the final Base-controlled activation signer before deployment.
pub const ACTIVATION_ADMIN_ADDRESS: Address =
    address!("0xcb00000000000000000000000000000000000000");

/// Security-token factory creation feature id.
pub const SECURITIES_TOKEN_CREATION: B256 =
    b256!("0x89e4523f0886ce01d76094212ed707081da92a45221e22c15c5689be470db63e");

/// Storage layout for the activation registry.
#[contract(addr = ACTIVATION_REGISTRY_ADDRESS)]
pub struct ActivationRegistryStorage {
    /// Runtime activation flags keyed by feature id.
    pub features: Mapping<B256, bool>,
}

/// Runtime activation registry for Base-native features.
#[derive(Debug, Default, Clone, Copy)]
pub struct ActivationRegistry;

impl ActivationRegistry {
    /// Creates a new activation registry handle.
    pub const fn new() -> Self {
        Self
    }

    /// Returns the activation admin.
    pub const fn activation_admin(self) -> Address {
        ACTIVATION_ADMIN_ADDRESS
    }
}
