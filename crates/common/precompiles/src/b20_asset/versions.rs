//! Version manager for the asset B-20 precompile.
//!
//! This module is the single owner of fork routing: which version is active at a
//! given hardfork ([`AssetVersions::from_base_upgrade`]), which concrete
//! implementation backs a version ([`AssetVersion::implementation`]), and which
//! wire surface it decodes against ([`AssetVersion::abi`]). Centralizing fork
//! routing here keeps hardfork logic auditable and off the execution path, and
//! lets the dispatcher route calls without ever matching on the version itself.

use alloc::string::ToString;

use alloy_sol_types::SolInterface;
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    Asset, AssetAccounting, AssetV1, AssetV2, IB20Asset, IB20AssetV1, IB20AssetV2, PolicyAccounting,
};

/// An activated version of the asset B-20 precompile logic.
///
/// Each variant maps to an immutable implementation via [`Self::implementation`] and to the wire
/// surface frozen at its fork via [`Self::abi`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetVersion {
    /// Introduced at Beryl, the asset's activation fork.
    V1,
    /// Introduced at Cobalt. Adds the ERC-8056 scheduled-multiplier surface
    V2,
}

impl AssetVersion {
    /// Returns the immutable logic implementation for this version.
    pub fn implementation<'l, S, A>(self) -> &'l dyn Asset<S, A>
    where
        S: AssetAccounting + 'l,
        A: PolicyAccounting + 'l,
    {
        static V1: AssetV1 = AssetV1;
        static V2: AssetV2 = AssetV2;
        match self {
            Self::V1 => &V1,
            Self::V2 => &V2,
        }
    }

    /// Returns the wire (ABI) surface frozen for this version.
    pub const fn abi(self) -> AssetAbi {
        match self {
            Self::V1 => AssetAbi::V1,
            Self::V2 => AssetAbi::V2,
        }
    }
}

/// A frozen wire (ABI) surface of the asset B-20 precompile. Reached only through
/// [`AssetVersion::abi`].
///
/// Covers only the asset-specific [`IB20Asset`] surface; the inherited [`crate::IB20`] surface is
/// shared across versions (see `b20_asset::abi`) and is decoded directly in the dispatcher's
/// fallthrough, not through this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetAbi {
    /// Wire surface activated at Beryl.
    V1,
    /// Wire surface activated at Cobalt.
    V2,
}

impl AssetAbi {
    /// Returns whether `selector` was dialable on this version's asset surface.
    pub fn asset_valid_selector(self, selector: [u8; 4]) -> bool {
        match self {
            Self::V1 => IB20AssetV1::IB20AssetCalls::valid_selector(selector),
            Self::V2 => IB20AssetV2::IB20AssetCalls::valid_selector(selector),
        }
    }

    /// Validates `calldata` against this version's asset surface, mapping failures to
    /// `AbiDecodeFailed`.
    pub fn abi_decode_validate_asset(self, calldata: &[u8], selector: [u8; 4]) -> Result<()> {
        match self {
            Self::V1 => IB20AssetV1::IB20AssetCalls::abi_decode_validate(calldata).map(|_| ()),
            Self::V2 => IB20AssetV2::IB20AssetCalls::abi_decode_validate(calldata).map(|_| ()),
        }
        .map_err(|error| BasePrecompileError::AbiDecodeFailed {
            selector,
            error: error.to_string(),
        })
    }

    /// Gates `calldata` on this version's asset surface, then decodes it into the canonical
    /// routable enum.
    pub fn decode_asset(self, calldata: &[u8]) -> Result<IB20Asset::IB20AssetCalls> {
        let Some(selector) = calldata.first_chunk::<4>().copied() else {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        };
        if !self.asset_valid_selector(selector) {
            return Err(BasePrecompileError::UnknownFunctionSelector(selector));
        }
        self.abi_decode_validate_asset(calldata, selector)?;

        IB20Asset::IB20AssetCalls::abi_decode_validate(calldata).map_err(|error| {
            BasePrecompileError::AbiDecodeFailed { selector, error: error.to_string() }
        })
    }
}

/// Resolver that selects the asset version active at a given hardfork.
///
/// The version is resolved once per call from the block's active upgrade; there
/// is only ever one active version at a time.
#[derive(Debug, Default, Clone, Copy)]
pub struct AssetVersions;

impl AssetVersions {
    /// Returns the version active at `upgrade`, or `None` before the introduction
    /// fork (Beryl), where the asset precompile is not installed at all.
    ///
    /// V1 is active from Beryl; V2 supersedes it from Cobalt.
    pub fn from_base_upgrade(upgrade: BaseUpgrade) -> Option<AssetVersion> {
        // Ordered thresholds rather than per-variant arms: a fork newer than Cobalt must inherit the
        // latest version (V2) until one supersedes it, and `BaseUpgrade` is `#[non_exhaustive]`, so
        // an explicit-variant match would need a wildcard that would wrongly send future forks to
        // `None`.
        match upgrade {
            u if u >= BaseUpgrade::Cobalt => Some(AssetVersion::V2),
            u if u >= BaseUpgrade::Beryl => Some(AssetVersion::V1),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_sol_types::{SolCall, SolInterface};
    use base_common_genesis::BaseUpgrade;

    use crate::{AssetAbi, AssetVersion, AssetVersions, IB20Asset, IB20AssetV1, IB20AssetV2};

    #[test]
    fn resolves_none_before_beryl() {
        assert_eq!(AssetVersions::from_base_upgrade(BaseUpgrade::Azul), None);
    }

    #[test]
    fn resolves_v1_from_beryl() {
        assert_eq!(AssetVersions::from_base_upgrade(BaseUpgrade::Beryl), Some(AssetVersion::V1));
    }

    #[test]
    fn resolves_v2_at_cobalt() {
        assert_eq!(AssetVersions::from_base_upgrade(BaseUpgrade::Cobalt), Some(AssetVersion::V2));
    }

    /// The logic axis and the wire axis meet only here. Driven from the fork ladder so the whole
    /// chain (upgrade -> version -> surface) is pinned, not just the inner lookup.
    #[test]
    fn each_fork_resolves_to_its_wire_surface() {
        assert_eq!(AssetVersion::V1.abi(), AssetAbi::V1);
        assert_eq!(AssetVersion::V2.abi(), AssetAbi::V2);

        let beryl = AssetVersions::from_base_upgrade(BaseUpgrade::Beryl).unwrap();
        let cobalt = AssetVersions::from_base_upgrade(BaseUpgrade::Cobalt).unwrap();
        assert_eq!(beryl.abi(), AssetAbi::V1);
        assert_eq!(cobalt.abi(), AssetAbi::V2);
    }

    /// `SolInterface::NAME` lands in consensus data: the short-calldata branch of
    /// `abi_decode_validate` builds its error from it, and `AbiDecodeFailed` puts that string on
    /// the wire. Renaming either frozen interface would change historical revert payloads.
    #[test]
    fn surface_interface_names_are_frozen() {
        assert_eq!(IB20AssetV1::IB20AssetCalls::NAME, "IB20AssetCalls");
        assert_eq!(IB20AssetV2::IB20AssetCalls::NAME, "IB20AssetCalls");
    }

    /// `abi_decode_validate` short-circuits on `len < MIN_DATA_LENGTH + 4` before looking at the
    /// selector. Equal minimums across surfaces is what makes truncated calldata for a *shared*
    /// selector produce identical `AbiDecodeFailed` bytes at every fork, and it is not obvious from
    /// the interface definitions. Both surfaces have zero-arg calls, so this is `0` on each.
    #[test]
    fn surfaces_share_a_minimum_calldata_length() {
        assert_eq!(
            IB20AssetV1::IB20AssetCalls::MIN_DATA_LENGTH,
            IB20AssetV2::IB20AssetCalls::MIN_DATA_LENGTH
        );
    }

    /// The dispatcher re-decodes against the canonical surface after a frozen surface accepts, so
    /// every frozen selector must exist on canonical. The difference is exactly the 8 ERC-8056
    /// scheduled-multiplier selectors Cobalt introduced.
    #[test]
    fn v1_selectors_are_a_subset_of_v2() {
        let v1: Vec<[u8; 4]> = IB20AssetV1::IB20AssetCalls::selectors().collect();
        for selector in &v1 {
            assert!(
                AssetAbi::V2.asset_valid_selector(*selector),
                "V1 selector {selector:?} missing from the V2 surface"
            );
        }

        let added: Vec<[u8; 4]> = IB20AssetV2::IB20AssetCalls::selectors()
            .filter(|selector| !AssetAbi::V1.asset_valid_selector(*selector))
            .collect();
        assert_eq!(added.len(), 8);
        for selector in [
            IB20Asset::uiMultiplierCall::SELECTOR,
            IB20Asset::newUIMultiplierCall::SELECTOR,
            IB20Asset::effectiveAtCall::SELECTOR,
            IB20Asset::balanceOfUICall::SELECTOR,
            IB20Asset::totalSupplyUICall::SELECTOR,
            IB20Asset::setUIMultiplierCall::SELECTOR,
            IB20Asset::cancelScheduledMultiplierCall::SELECTOR,
            IB20Asset::supportsInterfaceCall::SELECTOR,
        ] {
            assert!(
                added.contains(&selector),
                "expected scheduled selector {selector:?} in the V2 delta"
            );
        }
    }
}
