//! Contract-backed upgrade ID helpers.

use base_common_genesis::BaseUpgrade;

/// Formats the contract-backed upgrade IDs in registration order.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct ContractUpgradeIds;

impl ContractUpgradeIds {
    /// Returns the contract-backed upgrade IDs as a comma-separated list.
    pub fn csv() -> String {
        BaseUpgrade::CONTRACT_VARIANTS
            .iter()
            .map(|upgrade| upgrade.contract_id())
            .collect::<Vec<_>>()
            .join(",")
    }
}
