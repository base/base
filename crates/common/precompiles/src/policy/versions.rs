//! Version manager for the `PolicyRegistry` precompile.
//!
//! Single owner of both version mappings: which version is active at a given hardfork
//! ([`VersionResolver::from_base_upgrade`]), and which concrete implementation backs a
//! version ([`Version::implementation`]). Centralizing fork routing here keeps
//! hardfork logic auditable and off the execution path, and lets the dispatcher route
//! calls without ever matching on the version itself.

use base_common_genesis::BaseUpgrade;

use super::logic::{PolicyRegistryLogic, PolicyRegistryLogicV1};
use crate::PolicyAccounting;

/// An activated version of the `PolicyRegistry` precompile logic.
///
/// Each variant maps to an immutable implementation via [`Self::implementation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// Introduced at Beryl, the fork where the policy registry precompile is installed.
    V1,
}

impl Version {
    /// Returns the immutable logic implementation for this version.
    pub fn implementation<'l, S>(self) -> &'l dyn PolicyRegistryLogic<S>
    where
        S: PolicyAccounting + 'l,
    {
        static V1: PolicyRegistryLogicV1 = PolicyRegistryLogicV1;
        match self {
            Self::V1 => &V1,
        }
    }
}

/// Resolver that selects the policy-registry version active at a given hardfork.
///
/// The version is resolved once per call from the block's active upgrade; there is only
/// ever one active version at a time.
#[derive(Debug, Default, Clone, Copy)]
pub struct VersionResolver;

impl VersionResolver {
    /// Returns the version active at `upgrade`, or `None` before Beryl, where the policy
    /// registry precompile is not installed at all.
    pub fn from_base_upgrade(upgrade: BaseUpgrade) -> Option<Version> {
        if upgrade >= BaseUpgrade::Beryl { Some(Version::V1) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::BaseUpgrade;

    use crate::{PolicyVersion, PolicyVersionResolver};

    #[test]
    fn resolves_none_before_beryl() {
        assert_eq!(PolicyVersionResolver::from_base_upgrade(BaseUpgrade::Azul), None);
    }

    #[test]
    fn resolves_v1_from_beryl() {
        assert_eq!(
            PolicyVersionResolver::from_base_upgrade(BaseUpgrade::Beryl),
            Some(PolicyVersion::V1)
        );
    }

    #[test]
    fn resolves_v1_at_cobalt() {
        assert_eq!(
            PolicyVersionResolver::from_base_upgrade(BaseUpgrade::Cobalt),
            Some(PolicyVersion::V1)
        );
    }
}
