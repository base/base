//! Version seam and resolver for the `foo` precompile.
//!
//! [`FooVersion`] is the entire shared interface between the precompile entry
//! and a version: a single `call` that takes calldata and returns encoded
//! output. Each version decodes and routes internally, so versions are
//! self-contained — the shared surface is one method plus [`FooVersions::resolve`].

use alloy_primitives::Bytes;
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{Result, StorageCtx};

use crate::{FooStorage, FooV1, FooV2};

/// The single seam every `foo` version implements.
///
/// `Sync` is required so a `&'static dyn FooVersion` can be captured by the
/// (thread-safe) precompile closure; the zero-sized version types satisfy it
/// trivially.
pub trait FooVersion: Sync {
    /// Decodes `calldata`, routes it to this version's logic, and returns the
    /// ABI-encoded output. Selectors this version does not implement revert
    /// with [`IFoo::UnsupportedBeforeActivation`](crate::IFoo::UnsupportedBeforeActivation).
    fn call(
        &self,
        storage: &mut FooStorage<'_>,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
    ) -> Result<Bytes>;
}

/// Resolver that selects the `foo` version active at a given hardfork.
///
/// Centralizing fork routing here keeps hardfork logic auditable and off the
/// execution path: the version is resolved once at install time. The versions
/// are zero-sized, so the returned reference is a pointer hand-back.
#[derive(Debug, Default, Clone, Copy)]
pub struct FooVersions;

impl FooVersions {
    /// Returns the version active at `upgrade`, or `None` before the
    /// introduction fork (Beryl), where `foo` is not installed at all.
    pub fn resolve(upgrade: BaseUpgrade) -> Option<&'static dyn FooVersion> {
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
    fn resolves_some_from_beryl() {
        assert!(FooVersions::resolve(BaseUpgrade::Beryl).is_some());
    }

    #[test]
    fn resolves_some_from_cobalt() {
        assert!(FooVersions::resolve(BaseUpgrade::Cobalt).is_some());
    }
}
