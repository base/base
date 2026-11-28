//! The B-20 token versions, the fork that activates each, and the selectors each version dials.
//!
//! The latest wire surfaces are the canonical [`IB20`] and [`IB20Asset`]; an older version dials
//! them minus the selectors a later fork added. Beryl's common surface also differs in one type,
//! its three-feature `PausableFeature`, kept as [`IB20V1`] so a V1 decode fails with Beryl's bytes.

use alloy_sol_types::{SolCall, SolInterface};
use base_common_genesis::{BaseFeature, BaseUpgrade};

use crate::{BasePrecompileError, Calldata, IB20, IB20Asset, IB20V1, Result};

/// An activated version of the B-20 token logic (asset and stablecoin alike).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum B20Version {
    /// Introduced at Beryl, the B-20 activation fork.
    V1,
    /// Introduced at Cobalt.
    V2,
    /// Introduced at Denim.
    V3,
}

impl B20Version {
    /// Common selectors Cobalt (V2) added: the seize surface.
    const B20_V2_SELECTORS: [[u8; 4]; 4] = [
        IB20::SEIZE_ROLECall::SELECTOR,
        IB20::SEIZE_EXEMPT_POLICYCall::SELECTOR,
        IB20::SEIZE_RECEIVER_POLICYCall::SELECTOR,
        IB20::seizeWithMemoCall::SELECTOR,
    ];

    /// Asset selectors Cobalt (V2) added: the ERC-8056 scheduled multiplier, its conversion
    /// aliases, `MAX_UI_MULTIPLIER` and ERC-165.
    const ASSET_V2_SELECTORS: [[u8; 4]; 11] = [
        IB20Asset::MAX_UI_MULTIPLIERCall::SELECTOR,
        IB20Asset::uiMultiplierCall::SELECTOR,
        IB20Asset::newUIMultiplierCall::SELECTOR,
        IB20Asset::effectiveAtCall::SELECTOR,
        IB20Asset::toUIAmountCall::SELECTOR,
        IB20Asset::fromUIAmountCall::SELECTOR,
        IB20Asset::balanceOfUICall::SELECTOR,
        IB20Asset::totalSupplyUICall::SELECTOR,
        IB20Asset::updateUIMultiplierCall::SELECTOR,
        IB20Asset::cancelUIMultiplierUpdateCall::SELECTOR,
        IB20Asset::supportsInterfaceCall::SELECTOR,
    ];

    /// The asset version active at `upgrade`, or `None` before Beryl, where the asset precompile
    /// is not installed. A fork newer than Denim inherits V3 until one supersedes it.
    pub const fn asset(upgrade: BaseUpgrade) -> Option<Self> {
        match upgrade {
            u if u.is_feature_enabled(BaseFeature::AssetPrecompileV3) => Some(Self::V3),
            u if u.is_feature_enabled(BaseFeature::AssetPrecompileV2) => Some(Self::V2),
            u if u.is_feature_enabled(BaseFeature::AssetPrecompile) => Some(Self::V1),
            _ => None,
        }
    }

    /// The stablecoin version active at `upgrade`, or `None` before Beryl, where the stablecoin
    /// precompile is not installed. A fork newer than Denim inherits V3 until one supersedes it.
    pub const fn stablecoin(upgrade: BaseUpgrade) -> Option<Self> {
        match upgrade {
            u if u.is_feature_enabled(BaseFeature::StablecoinPrecompileV3) => Some(Self::V3),
            u if u.is_feature_enabled(BaseFeature::StablecoinPrecompileV2) => Some(Self::V2),
            u if u.is_feature_enabled(BaseFeature::StablecoinPrecompile) => Some(Self::V1),
            _ => None,
        }
    }

    /// Returns whether `selector` is on this version's common `IB20` surface.
    pub fn dials_b20(self, selector: [u8; 4]) -> bool {
        IB20::IB20Calls::valid_selector(selector)
            && (self >= Self::V2 || !Self::B20_V2_SELECTORS.contains(&selector))
    }

    /// Returns whether `selector` is on this version's asset-specific `IB20Asset` surface. The
    /// asset and common surfaces are disjoint.
    pub fn dials_asset(self, selector: [u8; 4]) -> bool {
        IB20Asset::IB20AssetCalls::valid_selector(selector)
            && (self >= Self::V2 || !Self::ASSET_V2_SELECTORS.contains(&selector))
    }

    /// Decodes a call on this version's common surface. Once a selector is on the surface, a
    /// decode failure stays there. V1 first decodes against [`IB20V1`], so a `PausableFeature`
    /// beyond Beryl's three fails with Beryl's decode error.
    pub fn decode_b20(self, calldata: &[u8]) -> Result<IB20::IB20Calls> {
        let selector = Calldata::selector(calldata)?;
        if !self.dials_b20(selector) {
            return Err(BasePrecompileError::UnknownFunctionSelector(selector));
        }
        if self == Self::V1 && IB20V1::IB20Calls::valid_selector(selector) {
            Calldata::decode::<IB20V1::IB20Calls>(calldata, selector)?;
        }
        Calldata::decode(calldata, selector)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_primitives::Address;
    use alloy_sol_types::{SolCall, SolInterface};
    use base_common_genesis::BaseUpgrade;

    use crate::{B20Version, BasePrecompileError, Calldata, IB20, IB20Asset};

    const VERSIONS: [B20Version; 3] = [B20Version::V1, B20Version::V2, B20Version::V3];

    #[test]
    fn resolves_none_before_beryl() {
        assert_eq!(B20Version::asset(BaseUpgrade::Azul), None);
    }

    #[test]
    fn resolves_v1_from_beryl() {
        assert_eq!(B20Version::asset(BaseUpgrade::Beryl), Some(B20Version::V1));
    }

    #[test]
    fn resolves_v2_at_cobalt() {
        assert_eq!(B20Version::asset(BaseUpgrade::Cobalt), Some(B20Version::V2));
    }

    #[test]
    fn resolves_v3_from_denim() {
        assert_eq!(B20Version::asset(BaseUpgrade::Denim), Some(B20Version::V3));
        assert_eq!(B20Version::asset(BaseUpgrade::Zenith), Some(B20Version::V3));
    }

    #[test]
    fn stablecoin_resolves_none_before_beryl() {
        assert_eq!(B20Version::stablecoin(BaseUpgrade::Azul), None);
    }

    #[test]
    fn stablecoin_resolves_v1_from_beryl() {
        assert_eq!(B20Version::stablecoin(BaseUpgrade::Beryl), Some(B20Version::V1));
    }

    #[test]
    fn stablecoin_resolves_v2_at_cobalt() {
        assert_eq!(B20Version::stablecoin(BaseUpgrade::Cobalt), Some(B20Version::V2));
    }

    #[test]
    fn stablecoin_resolves_v3_from_denim() {
        assert_eq!(B20Version::stablecoin(BaseUpgrade::Denim), Some(B20Version::V3));
        assert_eq!(B20Version::stablecoin(BaseUpgrade::Zenith), Some(B20Version::V3));
    }

    /// Beryl's stablecoin dials the common surface without the seize selectors.
    #[test]
    fn stablecoin_each_fork_resolves_to_its_wire_surface() {
        let seize = IB20::seizeWithMemoCall::SELECTOR;
        assert!(!B20Version::stablecoin(BaseUpgrade::Beryl).unwrap().dials_b20(seize));
        for upgrade in [BaseUpgrade::Cobalt, BaseUpgrade::Denim] {
            assert!(B20Version::stablecoin(upgrade).unwrap().dials_b20(seize));
        }
    }

    /// Beryl dials neither the seize surface nor the ERC-8056 surface; Cobalt and Denim dial both.
    #[test]
    fn each_fork_resolves_to_its_wire_surface() {
        let seize = IB20::seizeWithMemoCall::SELECTOR;
        let ui_multiplier = IB20Asset::uiMultiplierCall::SELECTOR;
        let beryl = B20Version::asset(BaseUpgrade::Beryl).unwrap();
        assert!(!beryl.dials_b20(seize) && !beryl.dials_asset(ui_multiplier));
        for upgrade in [BaseUpgrade::Cobalt, BaseUpgrade::Denim] {
            let version = B20Version::asset(upgrade).unwrap();
            assert!(version.dials_b20(seize) && version.dials_asset(ui_multiplier));
        }
    }

    /// V1 dials the V2 surfaces minus the selectors Cobalt added (4 common, 11 asset).
    #[test]
    fn v1_selectors_are_a_subset_of_v2() {
        let added = IB20::IB20Calls::selectors().filter(|s| !B20Version::V1.dials_b20(*s));
        assert_eq!(added.count(), 4);
        let added: Vec<[u8; 4]> = IB20Asset::IB20AssetCalls::selectors()
            .filter(|selector| !B20Version::V1.dials_asset(*selector))
            .collect();
        assert_eq!(added.len(), 11);
        assert_eq!(IB20Asset::IB20AssetCalls::selectors().count() - added.len(), 12);
        for version in [B20Version::V2, B20Version::V3] {
            assert!(IB20::IB20Calls::selectors().all(|selector| version.dials_b20(selector)));
            assert!(IB20Asset::IB20AssetCalls::selectors().all(|s| version.dials_asset(s)));
        }
    }

    #[test]
    fn asset_and_common_selectors_are_disjoint_on_each_wire() {
        for version in VERSIONS {
            for selector in IB20Asset::IB20AssetCalls::selectors() {
                assert!(!version.dials_b20(selector), "{selector:?} owned by asset and common");
            }
            for selector in IB20::IB20Calls::selectors() {
                assert!(!version.dials_asset(selector), "{selector:?} owned by common and asset");
            }
        }
    }

    #[test]
    fn decode_rejects_cobalt_selectors_at_v1() {
        assert!(!B20Version::V1.dials_asset(IB20Asset::uiMultiplierCall::SELECTOR));
        let calldata = IB20::SEIZE_ROLECall {}.abi_encode();
        assert_eq!(
            B20Version::V1.decode_b20(&calldata),
            Err(BasePrecompileError::UnknownFunctionSelector(IB20::SEIZE_ROLECall::SELECTOR))
        );
    }

    #[test]
    fn decode_accepts_cobalt_asset_selector_at_v2() {
        let calldata = IB20Asset::uiMultiplierCall {}.abi_encode();
        for version in [B20Version::V2, B20Version::V3] {
            assert!(version.dials_asset(IB20Asset::uiMultiplierCall::SELECTOR));
            assert!(matches!(
                Calldata::decode(&calldata, IB20Asset::uiMultiplierCall::SELECTOR),
                Ok(IB20Asset::IB20AssetCalls::uiMultiplier(_))
            ));
        }
    }

    #[test]
    fn decode_accepts_common_selector_at_every_version() {
        let calldata =
            IB20::transferCall { to: Address::ZERO, amount: Default::default() }.abi_encode();
        for version in VERSIONS {
            assert!(matches!(version.decode_b20(&calldata), Ok(IB20::IB20Calls::transfer(_))));
        }
    }

    #[test]
    fn decode_malformed_known_common_call_is_abi_decode_failed() {
        let calldata = IB20::transferCall::SELECTOR.to_vec();
        let err = B20Version::V1.decode_b20(&calldata).unwrap_err();
        assert!(matches!(
            err,
            BasePrecompileError::AbiDecodeFailed { selector, .. }
                if selector == IB20::transferCall::SELECTOR
        ));
        assert_eq!(B20Version::V2.decode_b20(&calldata), Err(err.clone()));
        assert_eq!(B20Version::V3.decode_b20(&calldata), Err(err));
    }

    /// Beryl's `PausableFeature` has three features, so `SEIZE` (3) fails V1's decode.
    #[test]
    fn decode_rejects_seize_feature_at_v1_only() {
        let calldata = IB20::pauseCall { features: alloc::vec![IB20::PausableFeature::SEIZE] }
            .abi_encode();
        assert!(matches!(
            B20Version::V1.decode_b20(&calldata),
            Err(BasePrecompileError::AbiDecodeFailed { selector, .. })
                if selector == IB20::pauseCall::SELECTOR
        ));
        assert!(matches!(B20Version::V2.decode_b20(&calldata), Ok(IB20::IB20Calls::pause(_))));
    }

    /// A malformed call on an asset selector fails as that selector, never as a common call.
    #[test]
    fn decode_malformed_known_asset_call_does_not_fall_through_to_common() {
        let selector = IB20Asset::updateUIMultiplierCall::SELECTOR;
        let mut calldata = selector.to_vec();
        calldata.extend_from_slice(&[0u8; 8]);
        assert!(B20Version::V2.dials_asset(selector));
        assert!(matches!(
            Calldata::decode::<IB20Asset::IB20AssetCalls>(&calldata, selector),
            Err(BasePrecompileError::AbiDecodeFailed { selector: got, .. }) if got == selector
        ));
    }

    #[test]
    fn decode_unknown_selector_is_rejected() {
        let calldata = [0xde, 0xad, 0xbe, 0xef];
        for version in VERSIONS {
            assert!(!version.dials_asset(calldata));
            assert_eq!(
                version.decode_b20(&calldata),
                Err(BasePrecompileError::UnknownFunctionSelector(calldata))
            );
        }
    }

    #[test]
    fn decode_empty_calldata_is_unknown_selector() {
        assert_eq!(
            B20Version::V1.decode_b20(&[]),
            Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]))
        );
    }
}
