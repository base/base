//! The single wire (ABI) version axis for the whole B-20 family.
//!
//! The three B-20 wire surfaces — the shared [`IB20`], the asset-specific [`IB20Asset`], and the
//! stablecoin-specific [`IB20Stablecoin`] — version in lockstep at the same hardforks. [`B20Abi`]
//! is the one enum that names that shared version; each surface is decoded off it by an exhaustive
//! match, so adding a fork is a single [`B20Abi`] variant plus one arm per surface, and the
//! compiler blocks the build until every surface is filled in. A surface unchanged at a fork points
//! its arm at the previous frozen module (see e.g. `IB20StablecoinV2` aliasing `V1`).
//!
//! `B20Abi` is reached only through [`AssetVersion::abi`](crate::AssetVersion) /
//! [`StablecoinVersion::abi`](crate::StablecoinVersion): the version resolvers stay the sole readers
//! of the fork ladder, and this maps that logic version to the wire version (mirrors
//! `PolicyVersion::abi`).

use alloc::string::ToString;

use alloy_sol_types::SolInterface;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{IB20, IB20Asset, IB20AssetV1, IB20AssetV2, IB20Stablecoin, IB20V1, IB20V2};

/// Decodes `calldata` against the latest (canonical) wire surface `C`, whose frozen and canonical
/// forms are the same type, so a single decode suffices. A selector absent from `C` returns
/// `UnknownFunctionSelector`; malformed args return `AbiDecodeFailed`.
fn decode_latest<C: SolInterface>(calldata: &[u8]) -> Result<C> {
    let Some(selector) = calldata.first_chunk::<4>().copied() else {
        return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
    };
    if !C::valid_selector(selector) {
        return Err(BasePrecompileError::UnknownFunctionSelector(selector));
    }
    C::abi_decode_validate(calldata).map_err(|error| BasePrecompileError::AbiDecodeFailed {
        selector,
        error: error.to_string(),
    })
}

/// Decodes `calldata` against an older frozen wire surface `F` (a strict subset of the canonical
/// surface `C`), returning the canonical call type `C`.
///
/// A selector absent from `F` returns `UnknownFunctionSelector`. A present selector is validated
/// against `F` first — this is where a frozen surface rejects an enum discriminant or selector a
/// later fork added, keeping it undialable at earlier forks — and only then decoded against the
/// canonical surface `C`, so callers match one call type across versions. The latest version uses
/// [`decode_latest`] instead (its frozen surface is `C`), avoiding the second decode.
fn frozen_decode<F: SolInterface, C: SolInterface>(calldata: &[u8]) -> Result<C> {
    let Some(selector) = calldata.first_chunk::<4>().copied() else {
        return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
    };
    if !F::valid_selector(selector) {
        return Err(BasePrecompileError::UnknownFunctionSelector(selector));
    }
    F::abi_decode_validate(calldata).map_err(|error| BasePrecompileError::AbiDecodeFailed {
        selector,
        error: error.to_string(),
    })?;
    C::abi_decode_validate(calldata).map_err(|error| BasePrecompileError::AbiDecodeFailed {
        selector,
        error: error.to_string(),
    })
}

/// The frozen wire (ABI) version shared by the whole B-20 family. One bump here covers all three
/// surfaces; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum B20Abi {
    /// Wire surfaces frozen at Beryl, the B-20 activation fork (asset without the ERC-8056 surface).
    V1,
    /// Wire surfaces at Cobalt (asset with the ERC-8056 scheduled-multiplier surface).
    V2,
}

impl B20Abi {
    /// Decodes `calldata` against this version's shared [`IB20`] wire surface.
    pub fn ib20_decode(self, calldata: &[u8]) -> Result<IB20::IB20Calls> {
        match self {
            Self::V1 => frozen_decode::<IB20V1::IB20Calls, IB20::IB20Calls>(calldata),
            Self::V2 => decode_latest::<IB20::IB20Calls>(calldata),
        }
    }

    /// Decodes `calldata` against this version's asset-specific [`IB20Asset`] wire surface.
    pub fn asset_decode(self, calldata: &[u8]) -> Result<IB20Asset::IB20AssetCalls> {
        match self {
            Self::V1 => {
                frozen_decode::<IB20AssetV1::IB20AssetCalls, IB20Asset::IB20AssetCalls>(calldata)
            }
            Self::V2 => decode_latest::<IB20Asset::IB20AssetCalls>(calldata),
        }
    }

    /// Decodes `calldata` against this version's stablecoin-specific [`IB20Stablecoin`] surface.
    /// The stablecoin wire is unchanged across versions, so both decode the canonical surface.
    pub fn stablecoin_decode(self, calldata: &[u8]) -> Result<IB20Stablecoin::IB20StablecoinCalls> {
        match self {
            Self::V1 | Self::V2 => decode_latest::<IB20Stablecoin::IB20StablecoinCalls>(calldata),
        }
    }

    /// Whether `selector` was dialable on this version's shared [`IB20`] wire surface.
    pub fn ib20_valid_selector(self, selector: [u8; 4]) -> bool {
        match self {
            Self::V1 => IB20V1::IB20Calls::valid_selector(selector),
            Self::V2 => IB20V2::IB20Calls::valid_selector(selector),
        }
    }

    /// Whether `selector` was dialable on this version's asset-specific [`IB20Asset`] surface.
    pub fn asset_valid_selector(self, selector: [u8; 4]) -> bool {
        match self {
            Self::V1 => IB20AssetV1::IB20AssetCalls::valid_selector(selector),
            Self::V2 => IB20AssetV2::IB20AssetCalls::valid_selector(selector),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::{SolCall, SolInterface};
    use base_common_genesis::BaseUpgrade;

    use crate::{
        AssetVersion, AssetVersions, B20Abi, IB20Asset, IB20AssetV1, IB20AssetV2, IB20StablecoinV1,
        IB20StablecoinV2, IB20V1, IB20V2, StablecoinVersion, StablecoinVersions,
    };

    /// The logic axis and the wire axis meet only here: every fork resolves to one family wire
    /// version, and both variants map to the same one.
    #[test]
    fn each_fork_resolves_to_one_family_version() {
        assert_eq!(AssetVersion::V1.abi(), B20Abi::V1);
        assert_eq!(AssetVersion::V2.abi(), B20Abi::V2);
        assert_eq!(StablecoinVersion::V1.abi(), B20Abi::V1);
        assert_eq!(StablecoinVersion::V2.abi(), B20Abi::V2);

        let asset = AssetVersions::from_base_upgrade(BaseUpgrade::Beryl).unwrap();
        let stable = StablecoinVersions::from_base_upgrade(BaseUpgrade::Cobalt).unwrap();
        assert_eq!(asset.abi(), B20Abi::V1);
        assert_eq!(stable.abi(), B20Abi::V2);
    }

    /// `SolInterface::NAME` lands in consensus data via the short-calldata `AbiDecodeFailed` path, so
    /// every frozen surface keeps its interface Rust name across versions.
    #[test]
    fn surface_interface_names_are_frozen() {
        assert_eq!(IB20V1::IB20Calls::NAME, "IB20Calls");
        assert_eq!(IB20V2::IB20Calls::NAME, "IB20Calls");
        assert_eq!(IB20AssetV1::IB20AssetCalls::NAME, "IB20AssetCalls");
        assert_eq!(IB20AssetV2::IB20AssetCalls::NAME, "IB20AssetCalls");
        assert_eq!(IB20StablecoinV1::IB20StablecoinCalls::NAME, "IB20StablecoinCalls");
        assert_eq!(IB20StablecoinV2::IB20StablecoinCalls::NAME, "IB20StablecoinCalls");
    }

    /// Equal minimum calldata lengths across a surface's versions keep truncated-calldata reverts
    /// byte-identical at every fork.
    #[test]
    fn surfaces_share_a_minimum_calldata_length() {
        assert_eq!(IB20V1::IB20Calls::MIN_DATA_LENGTH, IB20V2::IB20Calls::MIN_DATA_LENGTH);
        assert_eq!(
            IB20AssetV1::IB20AssetCalls::MIN_DATA_LENGTH,
            IB20AssetV2::IB20AssetCalls::MIN_DATA_LENGTH
        );
    }

    /// The dispatcher re-decodes against the canonical surface after a frozen surface accepts, so
    /// every V1 selector must exist on V2. The shared `IB20` wire is unchanged so far (v1 == v2); a
    /// future fork that widens it adds selectors to the delta here.
    #[test]
    fn ib20_v1_selectors_are_a_subset_of_v2() {
        for selector in IB20V1::IB20Calls::selectors() {
            assert!(
                B20Abi::V2.ib20_valid_selector(selector),
                "V1 selector {selector:?} missing from the V2 surface"
            );
        }
        let added = IB20V2::IB20Calls::selectors()
            .filter(|selector| !B20Abi::V1.ib20_valid_selector(*selector))
            .count();
        assert_eq!(added, 0);
    }

    /// For the asset surface the V1->V2 difference is exactly the 8 ERC-8056 selectors, absent at V1
    /// and present at V2. This is what replaces the hand-written ERC-8056 fork gate in dispatch.
    #[test]
    fn asset_v1_selectors_are_a_subset_of_v2() {
        for selector in IB20AssetV1::IB20AssetCalls::selectors() {
            assert!(
                B20Abi::V2.asset_valid_selector(selector),
                "asset V1 selector {selector:?} missing from the V2 surface"
            );
        }
        let added = IB20AssetV2::IB20AssetCalls::selectors()
            .filter(|selector| !B20Abi::V1.asset_valid_selector(*selector))
            .count();
        assert_eq!(added, 8);
        assert!(!B20Abi::V1.asset_valid_selector(IB20Asset::setUIMultiplierCall::SELECTOR));
        assert!(B20Abi::V2.asset_valid_selector(IB20Asset::setUIMultiplierCall::SELECTOR));
    }
}
