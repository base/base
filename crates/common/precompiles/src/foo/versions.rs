//! Central version manager for the `foo` precompile.
//!
//! This module is the single owner of both version mappings: which version is
//! active at a given hardfork ([`FooVersions::resolve`]), and which concrete
//! implementation backs a version ([`FooVersion::logic`]). Everything else —
//! the entry point and the dispatcher — depends only on these two seams and
//! never matches on the version itself.

use base_common_genesis::BaseUpgrade;

use crate::{FooLogic, FooV1, FooV2, FooV3};

/// An activated version of the `foo` precompile logic.
///
/// Each variant maps to an immutable implementation via [`Self::logic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooVersion {
    /// Introduced at Beryl.
    V1,
    /// Introduced at Cobalt: changes `helloWorld` and adds `greet`.
    V2,
    /// Staged for the next hardfork: a self-contained copy that changes `greet`.
    V3,
}

impl FooVersion {
    /// Returns the immutable logic implementation for this version.
    ///
    /// This is the one place that maps a version to its concrete
    /// implementation; callers route ABI calls through the returned
    /// [`FooLogic`] without ever matching on the version themselves. The
    /// implementations are zero-sized statics, so this is a pointer hand-back,
    /// not an allocation.
    pub fn logic(self) -> &'static dyn FooLogic {
        static FOO_V1: FooV1 = FooV1;
        static FOO_V2: FooV2 = FooV2 { previous: FooV1 };
        static FOO_V3: FooV3 = FooV3;
        match self {
            Self::V1 => &FOO_V1,
            Self::V2 => &FOO_V2,
            Self::V3 => &FOO_V3,
        }
    }
}

/// Resolver that selects the `foo` version active at a given hardfork.
///
/// Centralizing fork routing here keeps hardfork logic auditable and off the
/// execution path: the version is resolved once at install time, not on every
/// call.
#[derive(Debug, Default, Clone, Copy)]
pub struct FooVersions;

impl FooVersions {
    /// Returns the version active at `upgrade`, or `None` before the
    /// introduction fork (Beryl), where `foo` is not installed at all.
    pub fn resolve(upgrade: BaseUpgrade) -> Option<FooVersion> {
        // V3 is staged behind the permanently-off `Zombie` gate: wired up and
        // testable, but never selected on a live chain until this branch is
        // repointed to a real, scheduled fork.
        if upgrade >= BaseUpgrade::Zombie {
            Some(FooVersion::V3)
        } else if upgrade >= BaseUpgrade::Cobalt {
            Some(FooVersion::V2)
        } else if upgrade >= BaseUpgrade::Beryl {
            Some(FooVersion::V1)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::BaseUpgrade;

    use crate::{FooVersion, FooVersions};

    #[test]
    fn resolves_none_before_beryl() {
        assert_eq!(FooVersions::resolve(BaseUpgrade::Azul), None);
    }

    #[test]
    fn resolves_v1_from_beryl() {
        assert_eq!(FooVersions::resolve(BaseUpgrade::Beryl), Some(FooVersion::V1));
    }

    #[test]
    fn resolves_v2_from_cobalt() {
        assert_eq!(FooVersions::resolve(BaseUpgrade::Cobalt), Some(FooVersion::V2));
    }

    #[test]
    fn resolves_v3_at_zombie_gate() {
        assert_eq!(FooVersions::resolve(BaseUpgrade::Zombie), Some(FooVersion::V3));
    }

    #[test]
    fn logic_hello_world_matches_version() {
        assert_eq!(FooVersion::V1.logic().hello_world(), "Hello, World!");
        assert_eq!(FooVersion::V2.logic().hello_world(), "Hello, World! Welcome to Base.");
        // V3 copies V2's `helloWorld` value verbatim.
        assert_eq!(FooVersion::V3.logic().hello_world(), "Hello, World! Welcome to Base.");
    }
}
