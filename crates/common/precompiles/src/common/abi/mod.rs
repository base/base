//! Versioned wire (ABI) surfaces for the shared `IB20` interface, one per hardfork that moved them.
//!
//! `IB20` is shared by the asset and stablecoin B-20 precompiles and is unversioned in Solidity,
//! but the wire it accepts need not be frozen forever: a hardfork may widen a `sol!` enum or add
//! selectors. A widened enum is decoded *before* version dispatch, so — unlike a brand-new selector,
//! which a caller cannot dial until a surface declares it — a new enum discriminant would otherwise
//! decode against the canonical surface at every historical fork. Freezing the surface each fork
//! shipped, and decoding calldata against the surface active at the block's fork, keeps historical
//! behavior byte-for-byte stable.
//!
//! The latest surface is always named `IB20` in its `vN` module, then re-exported here as both
//! [`IB20`] (canonical) and the highest `IB20VN`. Older forks keep the same `IB20` Rust name inside
//! their module so truncated-calldata revert bytes stay stable, and are re-exported as [`IB20V1`].
//!
//! Decoding is version-gated by [`B20Abi`](crate::B20Abi), the single family wire version, selected
//! per version by the asset/stablecoin version resolvers (`AssetVersion::abi` / `StablecoinVersion::abi`).

mod v1;
pub use v1::IB20 as IB20V1;

mod v2;
pub use v2::{IB20, IB20 as IB20V2};

impl IB20::IB20Calls {
    /// Returns the stable label for this decoded B-20 call.
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::name(_) => "precompile-b20-name",
            Self::symbol(_) => "precompile-b20-symbol",
            Self::decimals(_) => "precompile-b20-decimals",
            Self::totalSupply(_) => "precompile-b20-totalSupply",
            Self::balanceOf(_) => "precompile-b20-balanceOf",
            Self::allowance(_) => "precompile-b20-allowance",
            Self::supplyCap(_) => "precompile-b20-supplyCap",
            Self::nonces(_) => "precompile-b20-nonces",
            Self::contractURI(_) => "precompile-b20-contractURI",
            Self::DEFAULT_ADMIN_ROLE(_) => "precompile-b20-DEFAULT_ADMIN_ROLE",
            Self::MINT_ROLE(_) => "precompile-b20-MINT_ROLE",
            Self::BURN_ROLE(_) => "precompile-b20-BURN_ROLE",
            Self::BURN_BLOCKED_ROLE(_) => "precompile-b20-BURN_BLOCKED_ROLE",
            Self::PAUSE_ROLE(_) => "precompile-b20-PAUSE_ROLE",
            Self::UNPAUSE_ROLE(_) => "precompile-b20-UNPAUSE_ROLE",
            Self::METADATA_ROLE(_) => "precompile-b20-METADATA_ROLE",
            Self::TRANSFER_SENDER_POLICY(_) => "precompile-b20-TRANSFER_SENDER_POLICY",
            Self::TRANSFER_RECEIVER_POLICY(_) => "precompile-b20-TRANSFER_RECEIVER_POLICY",
            Self::TRANSFER_EXECUTOR_POLICY(_) => "precompile-b20-TRANSFER_EXECUTOR_POLICY",
            Self::MINT_RECEIVER_POLICY(_) => "precompile-b20-MINT_RECEIVER_POLICY",
            Self::hasRole(_) => "precompile-b20-hasRole",
            Self::getRoleAdmin(_) => "precompile-b20-getRoleAdmin",
            Self::pausedFeatures(_) => "precompile-b20-pausedFeatures",
            Self::policyId(_) => "precompile-b20-policyId",
            Self::isPaused(_) => "precompile-b20-isPaused",
            Self::DOMAIN_SEPARATOR(_) => "precompile-b20-DOMAIN_SEPARATOR",
            Self::eip712Domain(_) => "precompile-b20-eip712Domain",
            Self::transfer(_) => "precompile-b20-transfer",
            Self::transferFrom(_) => "precompile-b20-transferFrom",
            Self::approve(_) => "precompile-b20-approve",
            Self::transferWithMemo(_) => "precompile-b20-transferWithMemo",
            Self::transferFromWithMemo(_) => "precompile-b20-transferFromWithMemo",
            Self::mint(_) => "precompile-b20-mint",
            Self::mintWithMemo(_) => "precompile-b20-mintWithMemo",
            Self::burn(_) => "precompile-b20-burn",
            Self::burnWithMemo(_) => "precompile-b20-burnWithMemo",
            Self::burnBlocked(_) => "precompile-b20-burnBlocked",
            Self::pause(_) => "precompile-b20-pause",
            Self::unpause(_) => "precompile-b20-unpause",
            Self::updateSupplyCap(_) => "precompile-b20-updateSupplyCap",
            Self::updateName(_) => "precompile-b20-updateName",
            Self::updateSymbol(_) => "precompile-b20-updateSymbol",
            Self::updateContractURI(_) => "precompile-b20-updateContractURI",
            Self::grantRole(_) => "precompile-b20-grantRole",
            Self::revokeRole(_) => "precompile-b20-revokeRole",
            Self::renounceRole(_) => "precompile-b20-renounceRole",
            Self::renounceLastAdmin(_) => "precompile-b20-renounceLastAdmin",
            Self::setRoleAdmin(_) => "precompile-b20-setRoleAdmin",
            Self::updatePolicy(_) => "precompile-b20-updatePolicy",
            Self::permit(_) => "precompile-b20-permit",
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};

    use super::IB20;

    #[test]
    fn b20_call_labels_are_stable() {
        assert_eq!(
            IB20::IB20Calls::transfer(IB20::transferCall { to: Address::ZERO, amount: U256::ZERO })
                .as_label(),
            "precompile-b20-transfer"
        );
        assert_eq!(
            IB20::IB20Calls::updateSupplyCap(IB20::updateSupplyCapCall {
                newSupplyCap: U256::ZERO
            })
            .as_label(),
            "precompile-b20-updateSupplyCap"
        );
    }
}
