//! Versioned wire (ABI) surfaces for the shared `IB20` interface, one per hardfork that moved them.
//!
//! `IB20` is shared by the asset and stablecoin B-20 precompiles and is unversioned in Solidity,
//! but the wire it accepts is not: Cobalt widened `PausableFeature` with a `SEIZE` variant and
//! added the seize functions/getters. Unlike a new selector — which a caller cannot dial before the
//! surface declares it — a widened `sol!` enum is decoded *before* version dispatch, so at Beryl a
//! `pause`/`unpause`/`isPaused` carrying the `SEIZE` discriminant would silently decode against the
//! canonical surface and diverge from the frozen v1 behavior. Freezing the Beryl surface here and
//! decoding pre-Cobalt calldata against it closes that gap, the same way the selector gate closes it
//! for the seize functions.
//!
//! The latest surface is always named `IB20` in its `vN` module, then re-exported here as both
//! [`IB20`] (canonical) and [`IB20V2`]. Older forks keep the same `IB20` Rust name inside their
//! module so truncated-calldata revert bytes stay stable, and are re-exported as [`IB20V1`].
//!
//! Both surfaces are reached only through [`B20Abi`], selected per version by the asset/stablecoin
//! version resolvers (`AssetVersion::abi` / `StablecoinVersion::abi`).

use alloc::string::ToString;

use alloy_sol_types::SolInterface;
use base_precompile_storage::{BasePrecompileError, Result};

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
            Self::TRANSFER_FROM_SEIZABLE_ROLE(_) => "precompile-b20-TRANSFER_FROM_SEIZABLE_ROLE",
            Self::PAUSE_ROLE(_) => "precompile-b20-PAUSE_ROLE",
            Self::UNPAUSE_ROLE(_) => "precompile-b20-UNPAUSE_ROLE",
            Self::METADATA_ROLE(_) => "precompile-b20-METADATA_ROLE",
            Self::TRANSFER_SENDER_POLICY(_) => "precompile-b20-TRANSFER_SENDER_POLICY",
            Self::TRANSFER_RECEIVER_POLICY(_) => "precompile-b20-TRANSFER_RECEIVER_POLICY",
            Self::TRANSFER_EXECUTOR_POLICY(_) => "precompile-b20-TRANSFER_EXECUTOR_POLICY",
            Self::MINT_RECEIVER_POLICY(_) => "precompile-b20-MINT_RECEIVER_POLICY",
            Self::SEIZABLE_ACCOUNT_POLICY(_) => "precompile-b20-SEIZABLE_ACCOUNT_POLICY",
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
            Self::burnBlockedWithMemo(_) => "precompile-b20-burnBlockedWithMemo",
            Self::transferFromSeizableWithMemo(_) => "precompile-b20-transferFromSeizableWithMemo",
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

/// A frozen wire (ABI) surface of the shared `IB20` interface. Reached only through
/// [`AssetVersion::abi`](crate::AssetVersion) / [`StablecoinVersion::abi`](crate::StablecoinVersion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum B20Abi {
    /// Wire surface frozen at Beryl (no seize surface; `PausableFeature` has three variants).
    V1,
    /// Wire surface activated at Cobalt (adds the seize surface and the `SEIZE` pause feature).
    V2,
}

impl B20Abi {
    /// Returns whether `selector` was dialable on this wire surface.
    pub fn valid_selector(self, selector: [u8; 4]) -> bool {
        match self {
            Self::V1 => IB20V1::IB20Calls::valid_selector(selector),
            Self::V2 => IB20V2::IB20Calls::valid_selector(selector),
        }
    }

    /// Validates `calldata` against this wire surface via alloy's `abi_decode_validate`, discarding
    /// the decoded call. This is where the frozen V1 surface rejects the `SEIZE` discriminant: its
    /// `PausableFeature` has no index 3, so the enum type check fails and the call reverts with
    /// `AbiDecodeFailed` exactly as it did before Cobalt.
    pub fn abi_decode_validate(self, calldata: &[u8], selector: [u8; 4]) -> Result<()> {
        match self {
            Self::V1 => IB20V1::IB20Calls::abi_decode_validate(calldata).map(|_| ()),
            Self::V2 => IB20V2::IB20Calls::abi_decode_validate(calldata).map(|_| ()),
        }
        .map_err(|error| BasePrecompileError::AbiDecodeFailed {
            selector,
            error: error.to_string(),
        })
    }

    /// Decodes `calldata` into a routable canonical call, gated on this wire surface.
    ///
    /// A selector absent from this surface (the seize selectors at V1) returns
    /// `UnknownFunctionSelector`; a present selector is validated against the frozen surface first
    /// (rejecting e.g. the `SEIZE` discriminant at V1) and only then re-decoded against the
    /// canonical surface so the caller matches on one call type across versions.
    pub fn decode(self, calldata: &[u8]) -> Result<IB20::IB20Calls> {
        let Some(selector) = calldata.first_chunk::<4>().copied() else {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        };
        if !self.valid_selector(selector) {
            return Err(BasePrecompileError::UnknownFunctionSelector(selector));
        }
        self.abi_decode_validate(calldata, selector)?;

        IB20::IB20Calls::abi_decode_validate(calldata).map_err(|error| {
            BasePrecompileError::AbiDecodeFailed { selector, error: error.to_string() }
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_primitives::{Address, U256};
    use alloy_sol_types::{SolCall, SolInterface};
    use base_precompile_storage::BasePrecompileError;

    use super::{B20Abi, IB20, IB20V1, IB20V2};

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

    /// `SolInterface::NAME` lands in consensus data: the short-calldata branch of
    /// `abi_decode_validate` builds its error from it, and `AbiDecodeFailed` puts that string on the
    /// wire. Both frozen surfaces must keep the `IB20` Rust name.
    #[test]
    fn surface_interface_names_are_frozen() {
        assert_eq!(IB20V1::IB20Calls::NAME, "IB20Calls");
        assert_eq!(IB20V2::IB20Calls::NAME, "IB20Calls");
    }

    /// `abi_decode_validate` short-circuits on `len < MIN_DATA_LENGTH + 4` before looking at the
    /// selector. Equal minimums across surfaces is what makes truncated calldata produce identical
    /// bytes at every fork.
    #[test]
    fn surfaces_share_a_minimum_calldata_length() {
        assert_eq!(IB20V1::IB20Calls::MIN_DATA_LENGTH, IB20V2::IB20Calls::MIN_DATA_LENGTH);
    }

    /// The dispatcher re-decodes against the canonical surface after a frozen surface accepts, so
    /// every V1 selector must exist on canonical. The difference is exactly the seize surface Cobalt
    /// introduced.
    #[test]
    fn v1_selectors_are_a_subset_of_v2() {
        for selector in IB20V1::IB20Calls::selectors() {
            assert!(
                B20Abi::V2.valid_selector(selector),
                "V1 selector {selector:?} missing from the V2 surface"
            );
        }

        let added: Vec<[u8; 4]> = IB20V2::IB20Calls::selectors()
            .filter(|selector| !B20Abi::V1.valid_selector(*selector))
            .collect();
        assert_eq!(added.len(), 4);
        assert!(added.contains(&IB20::burnBlockedWithMemoCall::SELECTOR));
        assert!(added.contains(&IB20::transferFromSeizableWithMemoCall::SELECTOR));
        assert!(added.contains(&IB20::TRANSFER_FROM_SEIZABLE_ROLECall::SELECTOR));
        assert!(added.contains(&IB20::SEIZABLE_ACCOUNT_POLICYCall::SELECTOR));
    }

    /// The seize surface was not dialable at Beryl, so the V1 surface must not know its selectors.
    /// This is what replaces a hand-written fork gate in dispatch.
    #[test]
    fn v1_surface_rejects_seize_selectors() {
        for selector in [
            IB20::burnBlockedWithMemoCall::SELECTOR,
            IB20::transferFromSeizableWithMemoCall::SELECTOR,
            IB20::TRANSFER_FROM_SEIZABLE_ROLECall::SELECTOR,
            IB20::SEIZABLE_ACCOUNT_POLICYCall::SELECTOR,
        ] {
            assert!(!B20Abi::V1.valid_selector(selector));
            assert!(B20Abi::V2.valid_selector(selector));
        }
    }

    /// The bug this module fixes. `pause`/`unpause`/`isPaused` keep their selectors across the
    /// Cobalt widening, so only decoding against Beryl's own surface rejects the `SEIZE`
    /// discriminant. At V1 it must fail decode; at V2 it decodes to the `SEIZE` feature.
    #[test]
    fn v1_rejects_seize_discriminant_in_pause_path() {
        let is_paused = IB20::isPausedCall { feature: IB20::PausableFeature::SEIZE }.abi_encode();
        let pause =
            IB20::pauseCall { features: alloc::vec![IB20::PausableFeature::SEIZE] }.abi_encode();

        for calldata in [is_paused, pause] {
            // V1 rejects the discriminant at decode.
            assert!(matches!(
                B20Abi::V1.decode(&calldata),
                Err(BasePrecompileError::AbiDecodeFailed { .. })
            ));
            // V2 accepts it and yields a routable canonical call.
            assert!(B20Abi::V2.decode(&calldata).is_ok());
        }
    }
}
