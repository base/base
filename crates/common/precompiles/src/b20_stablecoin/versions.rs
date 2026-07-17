//! Version manager for the stablecoin B-20 precompile.
//!
//! This module is the single owner of both version mappings: which version is
//! active at a given hardfork ([`StablecoinVersions::from_base_upgrade`]), and which
//! concrete implementation backs a version ([`StablecoinVersion::implementation`]).
//! Centralizing fork routing here keeps hardfork logic auditable and off the
//! execution path, and lets the dispatcher route calls without ever matching on
//! the version itself.

use base_common_genesis::BaseUpgrade;

use crate::{Policy, Stablecoin, StablecoinAccounting, StablecoinV1};

/// An activated version of the stablecoin B-20 precompile logic.
///
/// Each variant maps to an immutable implementation via [`Self::implementation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StablecoinVersion {
    /// Introduced at Beryl, the stablecoin's activation fork.
    V1,
}

impl StablecoinVersion {
    /// Returns the immutable logic implementation for this version.
    pub fn implementation<'l, S, P>(self) -> &'l dyn Stablecoin<S, P>
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
    pub fn from_base_upgrade(upgrade: BaseUpgrade) -> Option<StablecoinVersion> {
        if upgrade >= BaseUpgrade::Beryl { Some(StablecoinVersion::V1) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::BaseUpgrade;

    use crate::{FrozenHash, StablecoinVersion, StablecoinVersions};

    /// SHA-256 of `logic/v1.rs`, pinned at Beryl activation. Recompute only as a
    /// deliberate, reviewed change alongside whatever edit to `logic/v1.rs` this
    /// hash is meant to gate.
    const V1_LOGIC_SOURCE: &[u8] = include_bytes!("logic/v1.rs");
    const V1_LOGIC_HASH: [u8; 32] =
        hex_literal::hex!("acfcb78949da613764339f58a89f60b8049ed9d3681dbc9a2db78f5aacbb13d8");

    #[test]
    fn v1_logic_is_frozen() {
        assert!(
            FrozenHash::matches(V1_LOGIC_SOURCE, V1_LOGIC_HASH),
            "logic/v1.rs changed since it was frozen at Beryl - if intentional, recompute its hash \
             and update V1_LOGIC_HASH as a deliberate, reviewed change"
        );
    }

    #[test]
    fn resolves_none_before_beryl() {
        assert_eq!(StablecoinVersions::from_base_upgrade(BaseUpgrade::Azul), None);
    }

    #[test]
    fn resolves_v1_from_beryl() {
        assert_eq!(
            StablecoinVersions::from_base_upgrade(BaseUpgrade::Beryl),
            Some(StablecoinVersion::V1)
        );
    }

    #[test]
    fn resolves_v1_at_cobalt() {
        assert_eq!(
            StablecoinVersions::from_base_upgrade(BaseUpgrade::Cobalt),
            Some(StablecoinVersion::V1)
        );
    }
}
