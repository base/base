//! The `PolicyRegistry` versions, the fork that activates each, and the selectors each dials:
//! Beryl's [`IPolicyRegistryV1`] surface, then the canonical [`IPolicyRegistry`] surface, without
//! Denim's `invertedPolicyId` at Cobalt.

use alloy_sol_types::{SolCall, SolInterface};
use base_common_genesis::{BaseFeature, BaseUpgrade};

use crate::{BasePrecompileError, Calldata, IPolicyRegistry, IPolicyRegistryV1, Result};

/// An activated version of the `PolicyRegistry` precompile logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PolicyVersion {
    /// Introduced at Beryl, the fork where the policy registry precompile is installed.
    V1,
    /// Introduced at Cobalt, superseding [`Self::V1`].
    V2,
    /// Introduced at Denim, superseding [`Self::V2`].
    V3,
}

impl PolicyVersion {
    /// Returns whether `selector` is on this version's surface.
    pub fn valid_selector(self, selector: [u8; 4]) -> bool {
        match self {
            Self::V1 => IPolicyRegistryV1::IPolicyRegistryCalls::valid_selector(selector),
            Self::V2 => {
                selector != IPolicyRegistry::invertedPolicyIdCall::SELECTOR
                    && IPolicyRegistry::IPolicyRegistryCalls::valid_selector(selector)
            }
            Self::V3 => IPolicyRegistry::IPolicyRegistryCalls::valid_selector(selector),
        }
    }

    /// Decodes a call on this version's surface. V1 first decodes against [`IPolicyRegistryV1`],
    /// so a composite `PolicyType` fails with Beryl's decode error.
    pub fn decode(self, calldata: &[u8]) -> Result<IPolicyRegistry::IPolicyRegistryCalls> {
        let selector = Calldata::selector(calldata)?;
        if !self.valid_selector(selector) {
            return Err(BasePrecompileError::UnknownFunctionSelector(selector));
        }
        if self == Self::V1 {
            Calldata::decode::<IPolicyRegistryV1::IPolicyRegistryCalls>(calldata, selector)?;
        }
        Calldata::decode(calldata, selector)
    }
}

/// Resolver that selects the policy-registry version active at a given hardfork.
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyVersions;

impl PolicyVersions {
    /// Returns the version active at `upgrade`, or `None` before Beryl.
    pub const fn from_base_upgrade(upgrade: BaseUpgrade) -> Option<PolicyVersion> {
        match upgrade {
            u if u.is_feature_enabled(BaseFeature::PolicyPrecompileV3) => Some(PolicyVersion::V3),
            u if u.is_feature_enabled(BaseFeature::PolicyPrecompileV2) => Some(PolicyVersion::V2),
            u if u.is_feature_enabled(BaseFeature::PolicyPrecompile) => Some(PolicyVersion::V1),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_sol_types::{SolCall, SolInterface};
    use base_common_genesis::BaseUpgrade;

    use crate::{IPolicyRegistry, IPolicyRegistryV1, PolicyVersion, PolicyVersions};

    #[test]
    fn resolves_none_before_beryl() {
        assert_eq!(PolicyVersions::from_base_upgrade(BaseUpgrade::Azul), None);
    }

    #[test]
    fn resolves_v1_from_beryl() {
        assert_eq!(PolicyVersions::from_base_upgrade(BaseUpgrade::Beryl), Some(PolicyVersion::V1));
    }

    #[test]
    fn resolves_v2_at_cobalt() {
        assert_eq!(PolicyVersions::from_base_upgrade(BaseUpgrade::Cobalt), Some(PolicyVersion::V2));
    }

    #[test]
    fn resolves_v3_from_denim() {
        assert_eq!(PolicyVersions::from_base_upgrade(BaseUpgrade::Denim), Some(PolicyVersion::V3));
        assert_eq!(PolicyVersions::from_base_upgrade(BaseUpgrade::Zenith), Some(PolicyVersion::V3));
    }

    /// The dispatcher re-decodes against the canonical surface after a frozen surface accepts, so
    /// every frozen selector must exist on the latest canonical surface. Cobalt adds the five
    /// composite selectors to V1; Denim adds `invertedPolicyId` to V2.
    #[test]
    fn v1_selectors_are_a_subset_of_v2() {
        let v1: Vec<[u8; 4]> = IPolicyRegistryV1::IPolicyRegistryCalls::selectors().collect();
        for selector in &v1 {
            assert!(
                PolicyVersion::V2.valid_selector(*selector),
                "V1 selector {selector:?} missing from the V2 surface"
            );
        }

        let added: Vec<[u8; 4]> = IPolicyRegistry::IPolicyRegistryCalls::selectors()
            .filter(|selector| PolicyVersion::V2.valid_selector(*selector))
            .filter(|selector| !PolicyVersion::V1.valid_selector(*selector))
            .collect();
        assert_eq!(added.len(), 5);
        assert!(added.contains(&IPolicyRegistry::createCompositePolicyCall::SELECTOR));
        assert!(added.contains(&IPolicyRegistry::updateCompositeCall::SELECTOR));
        assert!(added.contains(&IPolicyRegistry::compositePolicyChildIdsCall::SELECTOR));
        assert!(added.contains(&IPolicyRegistry::MIN_COMPOSITE_CHILD_POLICIESCall::SELECTOR));
        assert!(added.contains(&IPolicyRegistry::MAX_COMPOSITE_CHILD_POLICIESCall::SELECTOR));
    }

    #[test]
    fn v3_adds_only_inverted_policy_id() {
        let added: Vec<[u8; 4]> = IPolicyRegistry::IPolicyRegistryCalls::selectors()
            .filter(|selector| !PolicyVersion::V2.valid_selector(*selector))
            .collect();
        assert_eq!(added, vec![IPolicyRegistry::invertedPolicyIdCall::SELECTOR]);
    }

    /// `abi_decode_validate` short-circuits on `len < MIN_DATA_LENGTH + 4` before looking at the
    /// selector. Before Cobalt's `MIN_COMPOSITE_CHILD_POLICIES`/`MAX_COMPOSITE_CHILD_POLICIES`
    /// getters (the first zero-argument calls on either surface), equal minimums across surfaces
    /// meant truncated calldata produced identical bytes at every fork. V2's minimum is now `0`
    /// since those two calls take no arguments; V1, which never gained a zero-arg call, keeps `32`.
    #[test]
    fn surfaces_share_a_minimum_calldata_length() {
        assert_eq!(IPolicyRegistryV1::IPolicyRegistryCalls::MIN_DATA_LENGTH, 32);
        assert_eq!(IPolicyRegistry::IPolicyRegistryCalls::MIN_DATA_LENGTH, 0);
    }

    /// `SolInterface::NAME` lands in consensus data: the short-calldata branch of
    /// `abi_decode_validate` builds its error from it, and `AbiDecodeFailed` puts that string on
    /// the wire. Renaming either frozen interface would change historical revert payloads.
    #[test]
    fn surface_interface_names_are_frozen() {
        assert_eq!(IPolicyRegistryV1::IPolicyRegistryCalls::NAME, "IPolicyRegistryCalls");
        assert_eq!(IPolicyRegistry::IPolicyRegistryCalls::NAME, "IPolicyRegistryCalls");
    }

    /// The composite selectors were not dialable at Beryl, so the V1 surface must not know them.
    /// This is what replaced the hand-written fork gate in `dispatch`.
    #[test]
    fn v1_surface_rejects_composite_selectors() {
        assert!(
            !PolicyVersion::V1.valid_selector(IPolicyRegistry::createCompositePolicyCall::SELECTOR)
        );
        assert!(!PolicyVersion::V1.valid_selector(IPolicyRegistry::updateCompositeCall::SELECTOR));
        let child_ids = IPolicyRegistry::compositePolicyChildIdsCall::SELECTOR;
        assert!(!PolicyVersion::V1.valid_selector(child_ids));
        let create = IPolicyRegistry::createCompositePolicyCall::SELECTOR;
        assert!(PolicyVersion::V2.valid_selector(create));
        assert!(PolicyVersion::V2.valid_selector(IPolicyRegistry::updateCompositeCall::SELECTOR));
        assert!(
            PolicyVersion::V2.valid_selector(IPolicyRegistry::compositePolicyChildIdsCall::SELECTOR)
        );
    }
}
