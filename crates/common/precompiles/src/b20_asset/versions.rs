//! Version manager for the asset B-20 precompile.
//!
//! This module is the single owner of both version mappings: which version is
//! active at a given hardfork ([`AssetVersions::from_base_upgrade`]), and which
//! concrete implementation backs a version ([`AssetVersion::implementation`]).
//! Centralizing fork routing here keeps hardfork logic auditable and off the
//! execution path, and lets the dispatcher route calls without ever matching on
//! the version itself.

use base_common_genesis::BaseUpgrade;

use crate::{Asset, AssetAccounting, AssetV1, AssetV2, PolicyAccounting};

/// An activated version of the asset B-20 precompile logic.
///
/// Each variant maps to an immutable implementation via [`Self::implementation`]. Variants are
/// declared in activation order, so the derived ordering is chronological: `v < AssetVersion::V2`
/// means "a version that predates the Cobalt scheduled-multiplier surface", which the dispatcher
/// uses to gate those selectors out of earlier versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    use base_common_genesis::BaseUpgrade;

    use crate::{AssetVersion, AssetVersions};

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
}
