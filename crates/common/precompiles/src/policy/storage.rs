use alloy_primitives::{Address, address};
use base_precompile_macros::contract;
use base_precompile_storage::{Handler, Mapping, Result};

/// Storage layout for the `PolicyRegistry` precompile.
///
/// Slots are append-only — never reorder across hardforks.
#[contract(addr = Self::ADDRESS)]
pub struct PolicyRegistryStorage {
    pub members: Mapping<u64, Mapping<Address, bool>>, // slot 0
}

impl PolicyRegistryStorage<'_> {
    /// Singleton precompile address for the `PolicyRegistry`.
    pub const ADDRESS: Address = address!("b030000000000000000000000000000000000000");

    /// Returns `true` if `account` is authorized to send tokens under `policy_id`.
    pub(super) fn is_authorized(&self, policy_id: u64, account: Address) -> Result<bool> {
        self.members.at(&policy_id).at(&account).read()
    }
}
