//! Central version manager for the `foo` precompile.
//!
//! [`FooVersions::resolve`] is the single seam that maps a hardfork to the
//! immutable implementation active at that fork. Each version is a distinct,
//! self-contained type; nothing else in the module matches on "which version".

use base_common_genesis::BaseUpgrade;

use crate::{FooLogic, FooV1, FooV2};

/// Resolver that selects the `foo` implementation active at a given hardfork.
///
/// Centralizing fork routing here keeps hardfork logic auditable and off the
/// execution path: the implementation is resolved once at install time, not on
/// every call. The versions are zero-sized, so the returned reference is a
/// pointer hand-back, not an allocation.
#[derive(Debug, Default, Clone, Copy)]
pub struct FooVersions;

impl FooVersions {
    /// Returns the implementation active at `upgrade`, or `None` before the
    /// introduction fork (Beryl), where `foo` is not installed at all.
    pub fn resolve(upgrade: BaseUpgrade) -> Option<&'static dyn FooLogic> {
        if upgrade >= BaseUpgrade::Cobalt {
            Some(&FooV2)
        } else if upgrade >= BaseUpgrade::Beryl {
            Some(&FooV1)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::BaseUpgrade;

    use crate::FooVersions;

    #[test]
    fn resolves_none_before_beryl() {
        assert!(FooVersions::resolve(BaseUpgrade::Azul).is_none());
    }

    #[test]
    fn resolves_v1_from_beryl() {
        // `hello_world` fingerprints the active version.
        assert_eq!(FooVersions::resolve(BaseUpgrade::Beryl).unwrap().hello_world(), "Hello, World!");
    }

    #[test]
    fn resolves_v2_from_cobalt() {
        assert_eq!(
            FooVersions::resolve(BaseUpgrade::Cobalt).unwrap().hello_world(),
            "Hello, World! Welcome to Base."
        );
    }
}
