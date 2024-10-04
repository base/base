//! Address Types

use alloy_primitives::Address;

/// The set of network-specific contracts for a given chain.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Default)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "PascalCase"))]
pub struct AddressList {
    /// The address manager
    pub address_manager: Address,
    /// L1 Cross Domain Messenger proxy address
    pub l1_cross_domain_messenger_proxy: Address,
    /// L1 ERC721 Bridge proxy address
    #[cfg_attr(feature = "serde", serde(alias = "L1ERC721BridgeProxy"))]
    pub l1_erc721_bridge_proxy: Address,
    /// L1 Standard Bridge proxy address
    pub l1_standard_bridge_proxy: Address,
    /// L2 Output Oracle Proxy address
    pub l2_output_oracle_proxy: Option<Address>,
    /// Optimism Mintable ERC20 Factory Proxy address
    #[cfg_attr(feature = "serde", serde(alias = "OptimismMintableERC20FactoryProxy"))]
    pub optimism_mintable_erc20_factory_proxy: Address,
    /// Optimism Portal Proxy address
    pub optimism_portal_proxy: Address,
    /// System Config Proxy address
    pub system_config_proxy: Address,
    /// The system config owner
    pub system_config_owner: Address,
    /// Proxy Admin address
    pub proxy_admin: Address,
    /// The owner of the Proxy Admin
    pub proxy_admin_owner: Address,
    /// The guardian address
    pub guardian: Address,

    // Fault Proof Contract Addresses
    /// Anchor State Registry Proxy address
    pub anchor_state_registry_proxy: Option<Address>,
    /// Delayed WETH Proxy address
    #[cfg_attr(feature = "serde", serde(alias = "DelayedWETHProxy"))]
    pub delayed_weth_proxy: Option<Address>,
    /// Dispute Game Factory Proxy address
    pub dispute_game_factory_proxy: Option<Address>,
    /// Fault Dispute Game Proxy address
    pub fault_dispute_game: Option<Address>,
    /// MIPS Proxy address
    #[cfg_attr(feature = "serde", serde(alias = "MIPS"))]
    pub mips: Option<Address>,
    /// Permissioned Dispute Game Proxy address
    pub permissioned_dispute_game: Option<Address>,
    /// Preimage Oracle Proxy address
    pub preimage_oracle: Option<Address>,
    /// The challenger's address
    pub challenger: Option<Address>,
}

impl AddressList {
    /// Sets zeroed addresses to [`Option::None`].
    pub fn zero_proof_addresses(&mut self) {
        if self.anchor_state_registry_proxy == Some(Address::ZERO) {
            self.anchor_state_registry_proxy = None;
        }
        if self.delayed_weth_proxy == Some(Address::ZERO) {
            self.delayed_weth_proxy = None;
        }
        if self.dispute_game_factory_proxy == Some(Address::ZERO) {
            self.dispute_game_factory_proxy = None;
        }
        if self.fault_dispute_game == Some(Address::ZERO) {
            self.fault_dispute_game = None;
        }
        if self.mips == Some(Address::ZERO) {
            self.mips = None;
        }
        if self.permissioned_dispute_game == Some(Address::ZERO) {
            self.permissioned_dispute_game = None;
        }
        if self.preimage_oracle == Some(Address::ZERO) {
            self.preimage_oracle = None;
        }
        if self.challenger == Some(Address::ZERO) {
            self.challenger = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_proof_addresses() {
        let mut addresses = AddressList {
            anchor_state_registry_proxy: Some(Address::ZERO),
            delayed_weth_proxy: Some(Address::ZERO),
            dispute_game_factory_proxy: Some(Address::ZERO),
            fault_dispute_game: Some(Address::ZERO),
            mips: Some(Address::ZERO),
            permissioned_dispute_game: Some(Address::ZERO),
            preimage_oracle: Some(Address::ZERO),
            challenger: Some(Address::ZERO),
            ..Default::default()
        };

        addresses.zero_proof_addresses();

        assert_eq!(addresses.anchor_state_registry_proxy, None);
        assert_eq!(addresses.delayed_weth_proxy, None);
        assert_eq!(addresses.dispute_game_factory_proxy, None);
        assert_eq!(addresses.fault_dispute_game, None);
        assert_eq!(addresses.mips, None);
        assert_eq!(addresses.permissioned_dispute_game, None);
        assert_eq!(addresses.preimage_oracle, None);
        assert_eq!(addresses.challenger, None);
    }
}
