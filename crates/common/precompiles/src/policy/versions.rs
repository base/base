//! Version manager for the `PolicyRegistry` precompile.
//!
//! Single owner of fork routing: which version is active at a given hardfork
//! ([`PolicyVersions::from_base_upgrade`]), which concrete implementation backs a version
//! ([`PolicyVersion::implementation`]), and which wire surface it decodes against
//! ([`PolicyVersion::abi`]). Centralizing fork routing here keeps hardfork logic auditable and off
//! the execution path, and lets the dispatcher route calls without ever matching on the version
//! itself.
//!
//! # Two axes, one resolver
//!
//! Logic and the wire move at different rates: a fork can rewrite behavior without touching the
//! ABI, or widen a type without changing any selector. So [`PolicyVersion`] and [`PolicyAbi`] are
//! separate sequences, and a new [`PolicyAbi`] variant appears only when the wire actually moves.
//!
//! What keeps that safe is that [`PolicyVersion`] remains the *only* type that reads the fork
//! ladder. [`PolicyAbi`] deliberately has no `from_base_upgrade`: a second resolver over the same
//! ladder would be two maps that must agree, which is the failure this module exists to prevent.
//! With one resolver, both lookups are exhaustive matches over a single enum, so a new version is
//! a compile error in both arms until someone fills them in.

use alloc::string::ToString;

use alloy_sol_types::SolInterface;
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    IPolicyRegistry, IPolicyRegistryV1, IPolicyRegistryV2, IPolicyRegistryV3, PolicyAccounting,
    PolicyRegistryLogic, PolicyRegistryV1, PolicyRegistryV2,
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
    /// Introduced at Zombie. Worked example of a wire-only fork: the logic is still
    /// [`PolicyRegistryV2`], and only [`Self::abi`] moves.
    ///
    /// A version exists here even though no behavior changed, because [`Self::abi`] is a function
    /// of the version alone. Leaving Zombie on [`Self::V2`] would ask one version to answer with
    /// two surfaces.
    V3,
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
            // Zombie reuses Cobalt's frozen logic: `policyCount` reads the existing counter and
            // needs no version-specific behavior, so the fork adds no file under `logic/`.
            Self::V2 | Self::V3 => &V2,
        }
    }

    /// Returns the wire surface frozen alongside this version's logic.
    ///
    /// A version whose fork left the ABI untouched maps onto the previous surface; only a fork that
    /// moves the wire earns a new [`PolicyAbi`] variant.
    pub const fn abi(self) -> PolicyAbi {
        match self {
            Self::V1 => PolicyAbi::V1,
            Self::V2 => PolicyAbi::V2,
            Self::V3 => PolicyAbi::V3,
        }
    }
}

/// A frozen wire (ABI) surface of the `PolicyRegistry` precompile.
///
/// Reached only through [`PolicyVersion::abi`]; see the module docs for why there is no
/// `from_base_upgrade` here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyAbi {
    /// The Beryl surface: 11 functions, two-variant `PolicyType`.
    V1,
    /// The Cobalt surface: adds `createCompositePolicy`/`updateComposite` and the
    /// `UNION`/`INTERSECT` discriminants.
    V2,
    /// The Zombie surface: adds the `policyCount` read and nothing else.
    V3,
}

impl PolicyAbi {
    /// Returns whether `selector` was dialable on this wire surface.
    ///
    /// Selectors introduced by a later fork are absent here, so the dispatcher rejects them as
    /// unknown without needing a hand-written fork gate.
    pub fn valid_selector(self, selector: [u8; 4]) -> bool {
        match self {
            Self::V1 => IPolicyRegistryV1::IPolicyRegistryCalls::valid_selector(selector),
            Self::V2 => IPolicyRegistryV2::IPolicyRegistryCalls::valid_selector(selector),
            Self::V3 => IPolicyRegistryV3::IPolicyRegistryCalls::valid_selector(selector),
        }
    }

    /// Validates `calldata` against this wire surface, discarding the decoded call.
    ///
    /// The decoder's error text is consensus data: `AbiDecodeFailed` reverts with
    /// `selector || utf8(error)`. Decoding against the fork's own surface is what makes a Beryl
    /// `createPolicy(admin, UNION)` reproduce the pre-Cobalt revert rather than decoding cleanly
    /// and failing later with a different payload.
    ///
    /// The decoded value is dropped because
    /// [`route`](crate::PolicyRegistryStorage) re-decodes against the canonical surface. That is
    /// sound: every frozen surface accepts a subset of what canonical accepts, and produces the
    /// same value on that subset.
    pub fn abi_decode_validate(self, calldata: &[u8], selector: [u8; 4]) -> Result<()> {
        match self {
            Self::V1 => {
                IPolicyRegistryV1::IPolicyRegistryCalls::abi_decode_validate(calldata).map(|_| ())
            }
            Self::V2 => {
                IPolicyRegistryV2::IPolicyRegistryCalls::abi_decode_validate(calldata).map(|_| ())
            }
            Self::V3 => {
                IPolicyRegistryV3::IPolicyRegistryCalls::abi_decode_validate(calldata).map(|_| ())
            }
        }
        .map_err(|error| BasePrecompileError::AbiDecodeFailed {
            selector,
            error: error.to_string(),
        })
    }

    /// Decodes `calldata` into a routable call, gated on this wire surface.
    ///
    /// The frozen surface decides what is dialable and owns any error bytes; the canonical surface
    /// then produces the value the dispatcher matches on. Splitting it that way is what lets one
    /// routing table serve every version: a frozen surface accepts a subset of canonical's inputs
    /// and yields the same value on that subset, so the canonical decode can never disagree about a
    /// call the gate already admitted.
    ///
    /// Error shapes match the surrounding dispatch path: calldata too short to carry a selector and
    /// selectors absent from this surface are [`BasePrecompileError::UnknownFunctionSelector`];
    /// a dialable selector with undecodable arguments is
    /// [`BasePrecompileError::AbiDecodeFailed`].
    pub fn decode(self, calldata: &[u8]) -> Result<IPolicyRegistry::IPolicyRegistryCalls> {
        let Some(selector) = calldata.first_chunk::<4>().copied() else {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        };
        if !self.valid_selector(selector) {
            return Err(BasePrecompileError::UnknownFunctionSelector(selector));
        }
        self.abi_decode_validate(calldata, selector)?;

        IPolicyRegistry::IPolicyRegistryCalls::abi_decode_validate(calldata).map_err(|error| {
            BasePrecompileError::AbiDecodeFailed { selector, error: error.to_string() }
        })
    }
}

/// Resolver that selects the policy-registry version active at a given hardfork.
///
/// The version is resolved once per call from the block's active upgrade; there is only
/// ever one active version at a time.
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyVersions;

impl PolicyVersions {
    /// Returns the version active at `upgrade`, or `None` before Beryl, where the policy
    pub fn from_base_upgrade(upgrade: BaseUpgrade) -> Option<PolicyVersion> {
        if upgrade >= BaseUpgrade::Zombie {
            Some(PolicyVersion::V3)
        } else if upgrade >= BaseUpgrade::Cobalt {
            Some(PolicyVersion::V2)
        } else if upgrade >= BaseUpgrade::Beryl {
            Some(PolicyVersion::V1)
        } else {
            None
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
    /// every frozen selector must exist on canonical. The difference is exactly the two composite
    /// selectors Cobalt introduced.
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
        assert_eq!(added.len(), 2);
        assert!(added.contains(&IPolicyRegistry::createCompositePolicyCall::SELECTOR));
        assert!(added.contains(&IPolicyRegistry::updateCompositeCall::SELECTOR));
    }

    /// `abi_decode_validate` short-circuits on `len < MIN_DATA_LENGTH + 4` before looking at the
    /// selector. Equal minimums across surfaces is what makes truncated calldata produce identical
    /// bytes at every fork, and it is not obvious from the interface definitions.
    #[test]
    fn surfaces_share_a_minimum_calldata_length() {
        assert_eq!(
            IPolicyRegistryV1::IPolicyRegistryCalls::MIN_DATA_LENGTH,
            IPolicyRegistryV2::IPolicyRegistryCalls::MIN_DATA_LENGTH
        );
    }

    /// `SolInterface::NAME` lands in consensus data: the short-calldata branch of
    /// `abi_decode_validate` builds its error from it, and `AbiDecodeFailed` puts that string on
    /// the wire. Renaming either frozen interface would change historical revert payloads.
    #[test]
    fn surface_interface_names_are_frozen() {
        assert_eq!(IPolicyRegistryV1::IPolicyRegistryCalls::NAME, "IPolicyRegistryCalls");
        assert_eq!(IPolicyRegistryV2::IPolicyRegistryCalls::NAME, "IPolicyRegistryCalls");
    }

    /// The wire-only fork. Zombie earns a new [`PolicyVersion`] and a new [`PolicyAbi`], but no new
    /// logic: `policyCount` reads the existing counter, so V3 routes to the same frozen
    /// implementation Cobalt uses. This is the axis-independence the two enums exist for.
    #[test]
    fn zombie_moves_the_wire_without_moving_the_logic() {
        let cobalt = PolicyVersions::from_base_upgrade(BaseUpgrade::Cobalt).unwrap();
        let zombie = PolicyVersions::from_base_upgrade(BaseUpgrade::Zombie).unwrap();

        assert_eq!(cobalt, PolicyVersion::V2);
        assert_eq!(zombie, PolicyVersion::V3);

        // The wire surfaces differ...
        assert_ne!(cobalt.abi(), zombie.abi());
        assert_eq!(zombie.abi(), PolicyAbi::V3);

        // ...while `implementation` sends both to `PolicyRegistryV2`. The arm
        // `Self::V2 | Self::V3 => &V2` above is the whole cost of this fork on the logic axis.
        assert!(matches!(zombie, PolicyVersion::V3));
    }

    /// `policyCount` is dialable only from Zombie. Older surfaces reject the selector with no
    /// hand-written fork gate, which is what lets the defaulted trait method stay unreachable.
    #[test]
    fn policy_count_is_dialable_only_from_v3() {
        let selector = IPolicyRegistry::policyCountCall::SELECTOR;
        assert!(!PolicyAbi::V1.valid_selector(selector));
        assert!(!PolicyAbi::V2.valid_selector(selector));
        assert!(PolicyAbi::V3.valid_selector(selector));
    }

    /// The composite selectors were not dialable at Beryl, so the V1 surface must not know them.
    /// This is what replaced the hand-written fork gate in `dispatch`.
    #[test]
    fn v1_surface_rejects_composite_selectors() {
        assert!(
            !PolicyAbi::V1.valid_selector(IPolicyRegistry::createCompositePolicyCall::SELECTOR)
        );
        assert!(!PolicyAbi::V1.valid_selector(IPolicyRegistry::updateCompositeCall::SELECTOR));
        assert!(PolicyAbi::V2.valid_selector(IPolicyRegistry::createCompositePolicyCall::SELECTOR));
        assert!(PolicyAbi::V2.valid_selector(IPolicyRegistry::updateCompositeCall::SELECTOR));
    }
}
