use alloy_primitives::{Address, address};
use base_precompile_macros::contract;
use base_precompile_storage::{Handler, Mapping, Result};

/// Interface the token calls to make authorization decisions against the registry.
pub(super) trait PolicyStorage {
    /// Returns `true` if `account` is authorized to send tokens under `policy_id`.
    fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool>;
}

/// Singleton precompile address for the `PolicyRegistry`.
pub const POLICY_REGISTRY_ADDRESS: Address = address!("b030000000000000000000000000000000000000");

/// Storage layout for the `PolicyRegistry` precompile.
///
/// Slots are append-only — never reorder across hardforks.
#[contract(addr = POLICY_REGISTRY_ADDRESS)]
pub struct PolicyRegistryStorage {
    pub members: Mapping<u64, Mapping<Address, bool>>, // slot 0
}

impl PolicyStorage for PolicyRegistryStorage<'_> {
    fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool> {
        self.members.at(&policy_id).at(&account).read()
    }
}
