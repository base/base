//! Version manager for the B-20 token factory precompile.
//!
//! This module is the single owner of both version mappings: which version is
//! active at a given hardfork ([`VersionResolver::from_base_upgrade`]), and which
//! concrete implementation backs a version ([`Version::implementation`]).
//! Centralizing fork routing here keeps hardfork logic auditable and off the
//! execution path, and lets the dispatcher route calls without ever matching on
//! the version itself.

use base_common_genesis::BaseUpgrade;

use super::logic::{B20FactoryLogic, B20FactoryLogicV1};

/// An activated version of the B-20 token factory precompile logic.
///
/// Each variant maps to an immutable implementation via [`Self::implementation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// Introduced at Beryl, the factory's activation fork.
    V1,
}

impl Version {
    /// Returns the immutable logic implementation for this version.
    pub fn implementation<'l>(self) -> &'l dyn B20FactoryLogic {
        static V1: B20FactoryLogicV1 = B20FactoryLogicV1;
        match self {
            Self::V1 => &V1,
        }
    }
}

/// Resolver that selects the factory version active at a given hardfork.
///
/// The version is resolved once per call from the block's active upgrade; there
/// is only ever one active version at a time.
#[derive(Debug, Default, Clone, Copy)]
pub struct VersionResolver;

impl VersionResolver {
    /// Returns the version active at `upgrade`, or `None` before the introduction
    /// fork (Beryl), where the factory precompile is not installed at all.
    pub fn from_base_upgrade(upgrade: BaseUpgrade) -> Option<Version> {
        if upgrade >= BaseUpgrade::Beryl { Some(Version::V1) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::BaseUpgrade;

    use crate::{FactoryVersion, FactoryVersionResolver};

    #[test]
    fn resolves_none_before_beryl() {
        assert_eq!(FactoryVersionResolver::from_base_upgrade(BaseUpgrade::Azul), None);
    }

    #[test]
    fn resolves_v1_from_beryl() {
        assert_eq!(
            FactoryVersionResolver::from_base_upgrade(BaseUpgrade::Beryl),
            Some(FactoryVersion::V1)
        );
    }
}
