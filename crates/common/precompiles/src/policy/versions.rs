//! Version manager for the `PolicyRegistry` precompile.
//!
//! Single owner of fork routing: which version is active at a given hardfork
//! ([`PolicyVersions::from_base_upgrade`]), which concrete implementation backs a version
//! ([`PolicyVersion::implementation`]), and which wire surface it decodes against
//! ([`PolicyVersion::abi`]). Centralizing fork routing here keeps hardfork logic auditable and off
//! the execution path, and lets the dispatcher route calls without ever matching on the version
//! itself.

use alloc::string::ToString;

use alloy_sol_types::SolInterface;
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    IPolicyRegistry, IPolicyRegistryV1, IPolicyRegistryV2, PolicyAccounting, PolicyRegistryLogic,
    PolicyRegistryV1, PolicyRegistryV2,
};

/// An activated version of the `PolicyRegistry` precompile logic.
///
/// Each variant maps to an immutable implementation via [`Self::implementation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyVersion {
    /// Introduced at Beryl, the fork where the policy registry precompile is installed.
    V1,
    /// Introduced at Cobalt, superseding [`Self::V1`].
    V2,
}

impl PolicyVersion {
    /// Returns the immutable logic implementation for this version.
    pub fn implementation<'l, S>(self) -> &'l dyn PolicyRegistryLogic<S>
    where
        S: PolicyAccounting + 'l,
    {
        static V1: PolicyRegistryV1 = PolicyRegistryV1;
        static V2: PolicyRegistryV2 = PolicyRegistryV2;
        match self {
            Self::V1 => &V1,
            Self::V2 => &V2,
        }
    }
    /// Returns the wire (ABI) surface frozen for this version.
    pub const fn abi(self) -> PolicyAbi {
        match self {
            Self::V1 => PolicyAbi::V1,
            Self::V2 => PolicyAbi::V2,
        }
    }
}

/// A frozen wire (ABI) surface of the `PolicyRegistry` precompile. Reached only through [`PolicyVersion::abi`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyAbi {
    /// Wire surface activated at Beryl.
    V1,
    /// Wire surface activated at Cobalt.
    V2,
}

impl PolicyAbi {
    /// Returns whether `selector` was dialable on this wire surface.
    pub fn valid_selector(self, selector: [u8; 4]) -> bool {
        match self {
            Self::V1 => IPolicyRegistryV1::IPolicyRegistryCalls::valid_selector(selector),
            Self::V2 => IPolicyRegistryV2::IPolicyRegistryCalls::valid_selector(selector),
        }
    }

    /// Validates `calldata` against this wire surface via alloy's `abi_decode_validate`, discarding
    /// the decoded call. Used by the activation gate so malformed args fail before activation.
    pub fn abi_decode_validate(self, calldata: &[u8], selector: [u8; 4]) -> Result<()> {
        match self {
            Self::V1 => {
                IPolicyRegistryV1::IPolicyRegistryCalls::abi_decode_validate(calldata).map(|_| ())
            }
            Self::V2 => {
                IPolicyRegistryV2::IPolicyRegistryCalls::abi_decode_validate(calldata).map(|_| ())
            }
        }
        .map_err(|error| BasePrecompileError::AbiDecodeFailed {
            selector,
            error: error.to_string(),
        })
    }

    /// Decodes `calldata` into a routable call via alloy's `abi_decode`, gated on this wire surface.
    ///
    /// On V1 the frozen surface is decoded once and lifted into the canonical enum (no second ABI
    /// parse), so reject bytes stay V1's. V2 is already canonical and decodes once.
    pub fn decode(self, calldata: &[u8]) -> Result<IPolicyRegistry::IPolicyRegistryCalls> {
        let Some(selector) = calldata.first_chunk::<4>().copied() else {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        };
        if !self.valid_selector(selector) {
            return Err(BasePrecompileError::UnknownFunctionSelector(selector));
        }
        match self {
            Self::V1 => IPolicyRegistryV1::IPolicyRegistryCalls::abi_decode_validate(calldata)
                .map(Into::into)
                .map_err(|error| BasePrecompileError::AbiDecodeFailed {
                    selector,
                    error: error.to_string(),
                }),
            Self::V2 => IPolicyRegistry::IPolicyRegistryCalls::abi_decode_validate(calldata)
                .map_err(|error| BasePrecompileError::AbiDecodeFailed {
                    selector,
                    error: error.to_string(),
                }),
        }
    }
}

/// Resolver that selects the policy-registry version active at a given hardfork.
///
/// The version is resolved once per call from the block's active upgrade; there is only
/// ever one active version at a time.
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyVersions;

impl PolicyVersions {
    /// Returns the version active at `upgrade`, or `None` before the introduction
    pub fn from_base_upgrade(upgrade: BaseUpgrade) -> Option<PolicyVersion> {
        match upgrade {
            u if u >= BaseUpgrade::Cobalt => Some(PolicyVersion::V2),
            u if u >= BaseUpgrade::Beryl => Some(PolicyVersion::V1),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_sol_types::{SolCall, SolInterface};
    use base_common_genesis::BaseUpgrade;

    use crate::{
        IPolicyRegistry, IPolicyRegistryV1, IPolicyRegistryV2, PolicyAbi, PolicyVersion,
        PolicyVersions,
    };

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

    /// The logic axis and the wire axis meet only here. Driven from the fork ladder so the whole
    /// chain (upgrade -> version -> surface) is pinned, not just the inner lookup.
    #[test]
    fn each_fork_resolves_to_its_wire_surface() {
        assert_eq!(PolicyVersion::V1.abi(), PolicyAbi::V1);
        assert_eq!(PolicyVersion::V2.abi(), PolicyAbi::V2);

        let beryl = PolicyVersions::from_base_upgrade(BaseUpgrade::Beryl).unwrap();
        let cobalt = PolicyVersions::from_base_upgrade(BaseUpgrade::Cobalt).unwrap();
        assert_eq!(beryl.abi(), PolicyAbi::V1);
        assert_eq!(cobalt.abi(), PolicyAbi::V2);
    }

    /// The dispatcher re-decodes against the canonical surface after a frozen surface accepts, so
    /// every frozen selector must exist on canonical. The difference is exactly the three
    /// composite selectors Cobalt introduced.
    #[test]
    fn v1_selectors_are_a_subset_of_v2() {
        let v1: Vec<[u8; 4]> = IPolicyRegistryV1::IPolicyRegistryCalls::selectors().collect();
        for selector in &v1 {
            assert!(
                PolicyAbi::V2.valid_selector(*selector),
                "V1 selector {selector:?} missing from the V2 surface"
            );
        }

        let added: Vec<[u8; 4]> = IPolicyRegistryV2::IPolicyRegistryCalls::selectors()
            .filter(|selector| !PolicyAbi::V1.valid_selector(*selector))
            .collect();
        assert_eq!(added.len(), 5);
        assert!(added.contains(&IPolicyRegistry::createCompositePolicyCall::SELECTOR));
        assert!(added.contains(&IPolicyRegistry::updateCompositeCall::SELECTOR));
        assert!(added.contains(&IPolicyRegistry::compositePolicyChildIdsCall::SELECTOR));
        assert!(added.contains(&IPolicyRegistry::MIN_COMPOSITE_CHILD_POLICIESCall::SELECTOR));
        assert!(added.contains(&IPolicyRegistry::MAX_COMPOSITE_CHILD_POLICIESCall::SELECTOR));
    }

    /// `abi_decode_validate` short-circuits on `len < MIN_DATA_LENGTH + 4` before looking at the
    /// selector. Before Cobalt's `MIN_COMPOSITE_CHILD_POLICIES`/`MAX_COMPOSITE_CHILD_POLICIES`
    /// getters (the first zero-argument calls on either surface), equal minimums across surfaces
    /// meant truncated calldata produced identical bytes at every fork. V2's minimum is now `0`
    /// since those two calls take no arguments; V1, which never gained a zero-arg call, keeps `32`.
    #[test]
    fn surfaces_share_a_minimum_calldata_length() {
        assert_eq!(IPolicyRegistryV1::IPolicyRegistryCalls::MIN_DATA_LENGTH, 32);
        assert_eq!(IPolicyRegistryV2::IPolicyRegistryCalls::MIN_DATA_LENGTH, 0);
    }

    /// `SolInterface::NAME` lands in consensus data: the short-calldata branch of
    /// `abi_decode_validate` builds its error from it, and `AbiDecodeFailed` puts that string on
    /// the wire. Renaming either frozen interface would change historical revert payloads.
    #[test]
    fn surface_interface_names_are_frozen() {
        assert_eq!(IPolicyRegistryV1::IPolicyRegistryCalls::NAME, "IPolicyRegistryCalls");
        assert_eq!(IPolicyRegistryV2::IPolicyRegistryCalls::NAME, "IPolicyRegistryCalls");
    }

    /// The composite selectors were not dialable at Beryl, so the V1 surface must not know them.
    /// This is what replaced the hand-written fork gate in `dispatch`.
    #[test]
    fn v1_surface_rejects_composite_selectors() {
        assert!(
            !PolicyAbi::V1.valid_selector(IPolicyRegistry::createCompositePolicyCall::SELECTOR)
        );
        assert!(!PolicyAbi::V1.valid_selector(IPolicyRegistry::updateCompositeCall::SELECTOR));
        assert!(
            !PolicyAbi::V1.valid_selector(IPolicyRegistry::compositePolicyChildIdsCall::SELECTOR)
        );
        assert!(PolicyAbi::V2.valid_selector(IPolicyRegistry::createCompositePolicyCall::SELECTOR));
        assert!(PolicyAbi::V2.valid_selector(IPolicyRegistry::updateCompositeCall::SELECTOR));
        assert!(
            PolicyAbi::V2.valid_selector(IPolicyRegistry::compositePolicyChildIdsCall::SELECTOR)
        );
    }
}
