//! Version manager for the stablecoin B-20 precompile.
//!
//! This module is the single owner of both version mappings: which version is
//! active at a given hardfork ([`StablecoinVersions::resolve`]), and which
//! concrete implementation backs a version ([`StablecoinVersion::logic`]).
//! Centralizing fork routing here keeps hardfork logic auditable and off the
//! execution path, and lets the dispatcher route calls without ever matching on
//! the version itself.

use base_common_genesis::BaseUpgrade;

use crate::{Policy, StablecoinAccounting, StablecoinLogic, StablecoinV1};

/// An activated version of the stablecoin B-20 precompile logic.
///
/// Each variant maps to an immutable implementation via [`Self::logic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StablecoinVersion {
    /// Introduced at Beryl, the stablecoin's activation fork.
    V1,
}

impl StablecoinVersion {
    /// Returns the immutable logic implementation for this version.
    ///
    /// This is the one place that maps a version to its concrete implementation;
    /// callers route ABI calls through the returned [`StablecoinLogic`] without
    /// ever matching on the version themselves. The implementations are
    /// zero-sized statics, so this is a pointer hand-back, not an allocation.
    ///
    /// The returned reference is bound to `'l` rather than `'static` because the
    /// token's storage (`S`) and policy (`P`) adapters borrow the EVM context;
    /// the implementation itself holds no state, so any `'l` that outlives the
    /// call is sound.
    pub fn logic<'l, S, P>(self) -> &'l dyn StablecoinLogic<S, P>
    where
        S: StablecoinAccounting + 'l,
        P: Policy + 'l,
    {
        static V1: StablecoinV1 = StablecoinV1;
        match self {
            Self::V1 => &V1,
        }
    }
}

/// Resolver that selects the stablecoin version active at a given hardfork.
///
/// The version is resolved once per call from the block's active upgrade; there
/// is only ever one active version at a time.
#[derive(Debug, Default, Clone, Copy)]
pub struct StablecoinVersions;

impl StablecoinVersions {
    /// Returns the version active at `upgrade`, or `None` before the introduction
    /// fork (Beryl), where the stablecoin precompile is not installed at all.
    pub fn resolve(upgrade: BaseUpgrade) -> Option<StablecoinVersion> {
        if upgrade >= BaseUpgrade::Beryl {
            Some(StablecoinVersion::V1)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::BaseUpgrade;

    use crate::{StablecoinVersion, StablecoinVersions};

    #[test]
    fn resolves_none_before_beryl() {
        assert_eq!(StablecoinVersions::resolve(BaseUpgrade::Azul), None);
    }

    #[test]
    fn resolves_v1_from_beryl() {
        assert_eq!(StablecoinVersions::resolve(BaseUpgrade::Beryl), Some(StablecoinVersion::V1));
    }

    #[test]
    fn resolves_v1_at_cobalt() {
        assert_eq!(StablecoinVersions::resolve(BaseUpgrade::Cobalt), Some(StablecoinVersion::V1));
    }
}
