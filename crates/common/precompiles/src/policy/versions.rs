//! Version manager for the `PolicyRegistry` precompile.
//!
//! Single owner of both version mappings: which version is active at a given hardfork
//! ([`PolicyVersions::from_base_upgrade`]), and which concrete implementation backs a
//! version ([`PolicyVersion::implementation`]). Centralizing fork routing here keeps
//! hardfork logic auditable and off the execution path, and lets the dispatcher route
//! calls without ever matching on the version itself.

use base_common_genesis::BaseUpgrade;

use crate::{PolicyAccounting, PolicyRegistryLogic, PolicyRegistryV1};

/// An activated version of the `PolicyRegistry` precompile logic.
///
/// Each variant maps to an immutable implementation via [`Self::implementation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyVersion {
    /// Introduced at Beryl, the fork where the policy registry precompile is installed.
    V1,
}

impl PolicyVersion {
    /// Returns the immutable logic implementation for this version.
    pub fn implementation<'l, S>(self) -> &'l dyn PolicyRegistryLogic<S>
    where
        S: PolicyAccounting + 'l,
    {
        static V1: PolicyRegistryV1 = PolicyRegistryV1;
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
pub struct PolicyVersions;

impl PolicyVersions {
    /// Returns the version active at `upgrade`, or `None` before Beryl, where the policy
    /// registry precompile is not installed at all.
    pub fn from_base_upgrade(upgrade: BaseUpgrade) -> Option<PolicyVersion> {
        if upgrade >= BaseUpgrade::Beryl { Some(PolicyVersion::V1) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::BaseUpgrade;

    use crate::{PolicyVersion, PolicyVersions};

    #[test]
    fn resolves_none_before_beryl() {
        assert_eq!(PolicyVersions::from_base_upgrade(BaseUpgrade::Azul), None);
    }

    #[test]
    fn resolves_v1_from_beryl() {
        assert_eq!(PolicyVersions::from_base_upgrade(BaseUpgrade::Beryl), Some(PolicyVersion::V1));
    }

    #[test]
    fn resolves_v1_at_cobalt() {
        assert_eq!(PolicyVersions::from_base_upgrade(BaseUpgrade::Cobalt), Some(PolicyVersion::V1));
    }
}
