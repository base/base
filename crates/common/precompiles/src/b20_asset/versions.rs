//! Version manager for the asset B-20 precompile.
//!
//! This module is the single owner of fork routing: which version is active at a
//! given hardfork ([`AssetVersions::from_base_upgrade`]), which concrete
//! implementation backs a version ([`AssetVersion::implementation`]), and which
//! composite wire surface it decodes against ([`AssetVersion::abi`]). Centralizing
//! fork routing here keeps hardfork logic auditable and off the execution path, and
//! lets the dispatcher route calls without ever matching on the version itself.
//!
//! # Gate then decode
//!
//! [`AssetAbiPair::decode`] owns the full dialable surface for a version: the
//! versioned asset-specific [`IB20Asset`] selectors first, then the frozen common
//! [`B20Abi`] surface. Once a selector belongs to a surface, decode failure stays
//! on that surface — there is no fallthrough after a malformed known call.

use alloc::string::ToString;

use alloy_sol_types::SolInterface;
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    Asset, AssetAccounting, AssetV1, AssetV2, B20Abi, IB20, IB20Asset, IB20AssetV1, IB20AssetV2,
    PolicyAccounting,
};

/// An activated version of the asset B-20 precompile logic.
///
/// Each variant maps to an immutable implementation via [`Self::implementation`] and to the
/// composite wire surface frozen at its fork via [`Self::abi`].
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

    /// Returns the composite dialable surface frozen for this version.
    ///
    /// The join is exhaustive over [`AssetVersion`]: every logic version declares both its
    /// asset-extension ABI and the common [`B20Abi`] it decodes against. There is no independent
    /// common fork ladder.
    pub const fn abi(self) -> AssetAbiPair {
        match self {
            Self::V1 => AssetAbiPair { asset: AssetAbi::V1, common_b20: B20Abi::V1 },
            Self::V2 => AssetAbiPair { asset: AssetAbi::V2, common_b20: B20Abi::V2 },
        }
    }
}

/// A frozen wire (ABI) surface of the asset-specific B-20 extension.
///
/// Reached only through [`AssetVersion::abi`]. Covers only asset-extension selectors;
/// the inherited common surface is selected separately as [`B20Abi`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetAbi {
    /// Wire surface activated at Beryl.
    V1,
    /// Wire surface activated at Cobalt.
    V2,
}

impl AssetAbi {
    /// Returns whether `selector` was dialable on this version's asset-specific surface.
    pub fn valid_selector(self, selector: [u8; 4]) -> bool {
        match self {
            Self::V1 => IB20AssetV1::IB20AssetCalls::valid_selector(selector),
            Self::V2 => IB20AssetV2::IB20AssetCalls::valid_selector(selector),
        }
    }

    /// Decodes `calldata` into a routable asset call, gated on this extension surface.
    ///
    /// On V1 the frozen surface is decoded once and lifted into the canonical enum (no second ABI
    /// parse), so reject bytes stay V1's and successful calls avoid the double owned materialization
    fn decode(self, calldata: &[u8]) -> Result<IB20Asset::IB20AssetCalls> {
        let Some(selector) = calldata.first_chunk::<4>().copied() else {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        };
        if !self.valid_selector(selector) {
            return Err(BasePrecompileError::UnknownFunctionSelector(selector));
        }
        match self {
            Self::V1 => IB20AssetV1::IB20AssetCalls::abi_decode_validate(calldata)
                .map(Into::into)
                .map_err(|error| BasePrecompileError::AbiDecodeFailed {
                    selector,
                    error: error.to_string(),
                }),
            Self::V2 => IB20Asset::IB20AssetCalls::abi_decode_validate(calldata).map_err(|error| {
                BasePrecompileError::AbiDecodeFailed { selector, error: error.to_string() }
            }),
        }
    }
}

/// The complete dialable surface for an asset token version: extension + common.
///
/// Produced only by [`AssetVersion::abi`]. Asset selectors are tried first; common
/// selectors are the fallthrough. The two surfaces are disjoint, so a selector never
/// lands in both arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetAbiPair {
    /// Frozen asset-extension surface for this version.
    pub asset: AssetAbi,
    /// Frozen common B-20 surface for this version.
    pub common_b20: B20Abi,
}

/// A call decoded against the composite surface selected by [`AssetAbiPair`].
///
/// Crate-private routing envelope; not part of the public precompile API.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AssetCall {
    /// An asset-specific call from the frozen [`IB20Asset`] surface.
    Asset(IB20Asset::IB20AssetCalls),
    /// An inherited call from the frozen [`IB20`] surface.
    Common(IB20::IB20Calls),
}

impl AssetCall {
    /// Returns the stable metric label for this decoded call.
    pub(crate) const fn as_label(&self) -> &'static str {
        match self {
            Self::Asset(call) => call.as_label(),
            Self::Common(call) => call.as_label(),
        }
    }
}

impl AssetAbiPair {
    /// Decodes `calldata` into a routable call for this composite surface.
    pub(crate) fn decode(self, calldata: &[u8]) -> Result<AssetCall> {
        let Some(selector) = calldata.first_chunk::<4>().copied() else {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        };

        if self.asset.valid_selector(selector) {
            return self.asset.decode(calldata).map(AssetCall::Asset);
        }

        if self.common_b20.valid_selector(selector) {
            return self.common_b20.decode(calldata).map(AssetCall::Common);
        }

        Err(BasePrecompileError::UnknownFunctionSelector(selector))
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
    use alloc::string::ToString;
    use alloc::vec::Vec;

    use alloy_primitives::{Address, Bytes, U256};
    use alloy_sol_types::{SolCall, SolInterface};
    use base_common_genesis::BaseUpgrade;
    use base_precompile_storage::BasePrecompileError;

    use super::{AssetAbiPair, AssetCall};
    use crate::{
        AssetAbi, AssetVersion, AssetVersions, B20Abi, IB20, IB20Asset, IB20AssetV1, IB20AssetV2,
    };

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

    /// The logic axis and both wire axes meet only here. Driven from the fork ladder so the whole
    /// chain (upgrade -> version -> composite surface) is pinned, not just the inner lookups.
    #[test]
    fn each_fork_resolves_to_its_wire_surface() {
        assert_eq!(
            AssetVersion::V1.abi(),
            AssetAbiPair { asset: AssetAbi::V1, common_b20: B20Abi::V1 }
        );
        assert_eq!(
            AssetVersion::V2.abi(),
            AssetAbiPair { asset: AssetAbi::V2, common_b20: B20Abi::V2 }
        );

        let beryl = AssetVersions::from_base_upgrade(BaseUpgrade::Beryl).unwrap();
        let cobalt = AssetVersions::from_base_upgrade(BaseUpgrade::Cobalt).unwrap();
        assert_eq!(beryl.abi().asset, AssetAbi::V1);
        assert_eq!(beryl.abi().common_b20, B20Abi::V1);
        assert_eq!(cobalt.abi().asset, AssetAbi::V2);
        assert_eq!(cobalt.abi().common_b20, B20Abi::V2);
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

    /// The dispatcher lifts a frozen V1 decode into the canonical enum, so every frozen selector
    /// must exist on canonical. The difference is the 8 ERC-8056 scheduled-multiplier selectors
    /// Cobalt introduced plus the `toUIAmount` / `fromUIAmount` aliases and the
    /// `MAX_UI_MULTIPLIER` getter added by the interface review (11 total).
    #[test]
    fn v1_selectors_are_a_subset_of_v2() {
        let v1: Vec<[u8; 4]> = IB20AssetV1::IB20AssetCalls::selectors().collect();
        for selector in &v1 {
            assert!(
                AssetAbi::V2.valid_selector(*selector),
                "V1 selector {selector:?} missing from the V2 surface"
            );
        }

        let added: Vec<[u8; 4]> = IB20AssetV2::IB20AssetCalls::selectors()
            .filter(|selector| !AssetAbi::V1.valid_selector(*selector))
            .collect();
        assert_eq!(added.len(), 11);
        for selector in [
            IB20Asset::uiMultiplierCall::SELECTOR,
            IB20Asset::newUIMultiplierCall::SELECTOR,
            IB20Asset::effectiveAtCall::SELECTOR,
            IB20Asset::balanceOfUICall::SELECTOR,
            IB20Asset::totalSupplyUICall::SELECTOR,
            IB20Asset::updateUIMultiplierCall::SELECTOR,
            IB20Asset::cancelUIMultiplierUpdateCall::SELECTOR,
            IB20Asset::supportsInterfaceCall::SELECTOR,
            IB20Asset::toUIAmountCall::SELECTOR,
            IB20Asset::fromUIAmountCall::SELECTOR,
            IB20Asset::MAX_UI_MULTIPLIERCall::SELECTOR,
        ] {
            assert!(
                added.contains(&selector),
                "expected scheduled selector {selector:?} in the V2 delta"
            );
        }
    }

    #[test]
    fn asset_and_common_selectors_are_disjoint_on_each_wire() {
        for wire in [AssetVersion::V1.abi(), AssetVersion::V2.abi()] {
            for selector in IB20Asset::IB20AssetCalls::selectors() {
                if wire.asset.valid_selector(selector) {
                    assert!(
                        !wire.common_b20.valid_selector(selector),
                        "selector {selector:?} owned by both asset and common"
                    );
                }
            }
            for selector in IB20::IB20Calls::selectors() {
                if wire.common_b20.valid_selector(selector) {
                    assert!(
                        !wire.asset.valid_selector(selector),
                        "selector {selector:?} owned by both common and asset"
                    );
                }
            }
        }
    }

    #[test]
    fn decode_rejects_cobalt_asset_selector_at_v1() {
        let calldata = IB20Asset::uiMultiplierCall {}.abi_encode();
        assert_eq!(
            AssetVersion::V1.abi().decode(&calldata),
            Err(BasePrecompileError::UnknownFunctionSelector(
                IB20Asset::uiMultiplierCall::SELECTOR
            ))
        );
    }

    #[test]
    fn decode_accepts_cobalt_asset_selector_at_v2() {
        let calldata = IB20Asset::uiMultiplierCall {}.abi_encode();
        assert!(matches!(
            AssetVersion::V2.abi().decode(&calldata),
            Ok(AssetCall::Asset(IB20Asset::IB20AssetCalls::uiMultiplier(_)))
        ));
    }

    #[test]
    fn decode_accepts_common_selector_at_both_versions() {
        let calldata = IB20::nameCall {}.abi_encode();
        for version in [AssetVersion::V1, AssetVersion::V2] {
            assert!(matches!(
                version.abi().decode(&calldata),
                Ok(AssetCall::Common(IB20::IB20Calls::name(_)))
            ));
        }
    }

    #[test]
    fn decode_malformed_known_common_call_is_abi_decode_failed() {
        // `transfer(address,uint256)` with only the selector: owned by common, not asset.
        let calldata = IB20::transferCall::SELECTOR.to_vec();
        let err = AssetVersion::V1.abi().decode(&calldata).unwrap_err();
        assert!(matches!(
            err,
            BasePrecompileError::AbiDecodeFailed {
                selector,
                ..
            } if selector == IB20::transferCall::SELECTOR
        ));
        // Same bytes at V2: common V1 and V2 declare the same surface today.
        assert_eq!(AssetVersion::V2.abi().decode(&calldata), Err(err));
    }

    #[test]
    fn decode_malformed_known_asset_call_does_not_fall_through_to_common() {
        // Truncated `updateUIMultiplier` args: V2 owns the selector, so failure stays on asset.
        let mut calldata = IB20Asset::updateUIMultiplierCall::SELECTOR.to_vec();
        calldata.extend_from_slice(&[0u8; 8]);
        let err = AssetVersion::V2.abi().decode(&calldata).unwrap_err();
        assert!(matches!(
            err,
            BasePrecompileError::AbiDecodeFailed {
                selector,
                ..
            } if selector == IB20Asset::updateUIMultiplierCall::SELECTOR
        ));
    }

    #[test]
    fn decode_unknown_selector_is_rejected() {
        let calldata = [0xde, 0xad, 0xbe, 0xef];
        assert_eq!(
            AssetVersion::V1.abi().decode(&calldata),
            Err(BasePrecompileError::UnknownFunctionSelector(calldata))
        );
        assert_eq!(
            AssetVersion::V2.abi().decode(&calldata),
            Err(BasePrecompileError::UnknownFunctionSelector(calldata))
        );
    }

    #[test]
    fn decode_empty_calldata_is_unknown_selector() {
        assert_eq!(
            AssetVersion::V1.abi().decode(&[]),
            Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]))
        );
    }

    /// Frozen V1 decode + [`From`] lift must equal a direct canonical decode for every shared
    /// selector. Proves Layer 1 of Cantina #16 does not change the accept set or payload.
    #[test]
    fn v1_decode_lift_matches_canonical_for_shared_calls() {
        let samples: Vec<Vec<u8>> = alloc::vec![
            IB20Asset::multiplierCall {}.abi_encode(),
            IB20Asset::OPERATOR_ROLECall {}.abi_encode(),
            IB20Asset::WAD_PRECISIONCall {}.abi_encode(),
            IB20Asset::toScaledBalanceCall { rawBalance: U256::from(7u64) }.abi_encode(),
            IB20Asset::batchMintCall {
                recipients: alloc::vec![Address::repeat_byte(0x11)],
                amounts: alloc::vec![U256::from(9u64)],
            }
            .abi_encode(),
            IB20Asset::announceCall {
                internalCalls: alloc::vec![Bytes::from_static(&[0xde, 0xad])],
                id: alloc::string::String::from("id"),
                description: alloc::string::String::from("desc"),
                uri: alloc::string::String::from("uri"),
            }
            .abi_encode(),
            IB20Asset::updateExtraMetadataCall {
                key: alloc::string::String::from("k"),
                value: alloc::string::String::from("v"),
            }
            .abi_encode(),
        ];

        for calldata in samples {
            let lifted = AssetAbi::V1.decode(&calldata).expect("V1 must accept shared call");
            let canonical =
                IB20Asset::IB20AssetCalls::abi_decode_validate(&calldata).expect("canonical");
            assert_eq!(lifted, canonical);
        }
    }

    /// Truncated known asset selector: V1 error bytes come from the frozen surface, not canonical.
    #[test]
    fn v1_malformed_announce_keeps_frozen_error_bytes() {
        let calldata = IB20Asset::announceCall::SELECTOR.to_vec();
        let err = AssetAbi::V1.decode(&calldata).unwrap_err();
        let BasePrecompileError::AbiDecodeFailed { selector, error } = err else {
            panic!("expected AbiDecodeFailed, got {err:?}");
        };
        assert_eq!(selector, IB20Asset::announceCall::SELECTOR);
        let frozen_err =
            IB20AssetV1::IB20AssetCalls::abi_decode_validate(&calldata).unwrap_err().to_string();
        assert_eq!(error, frozen_err);
    }

    #[test]
    fn asset_call_labels_delegate_to_surface() {
        let asset =
            AssetCall::Asset(IB20Asset::IB20AssetCalls::multiplier(IB20Asset::multiplierCall {}));
        assert_eq!(asset.as_label(), "precompile-b20-asset-multiplier");

        let common = AssetCall::Common(IB20::IB20Calls::transfer(IB20::transferCall {
            to: Address::ZERO,
            amount: U256::ZERO,
        }));
        assert_eq!(common.as_label(), "precompile-b20-transfer");
    }
}
