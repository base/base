//! Central version manager for the `foo` precompile.

use base_common_genesis::BaseUpgrade;

/// An activated version of the `foo` precompile logic.
///
/// Each variant maps to an immutable implementation in [`crate::logic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooVersion {
    /// Introduced at Beryl.
    V1,
    /// Introduced at Cobalt: changes `helloWorld` and adds `greet`.
    V2,
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
        if upgrade >= BaseUpgrade::Cobalt {
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
}
