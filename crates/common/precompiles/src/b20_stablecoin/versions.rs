//! Version manager for the stablecoin B-20 precompile.
//!
//! This module is the single owner of both version mappings: which version is
//! active at a given hardfork ([`StablecoinVersions::from_base_upgrade`]), and which
//! concrete implementation backs a version ([`StablecoinVersion::implementation`]).
//! Centralizing fork routing here keeps hardfork logic auditable and off the
//! execution path, and lets the dispatcher route calls without ever matching on
//! the version itself.

use alloy_primitives::{Address, B256, Bytes};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{
    B20Abi, B20StablecoinToken, PolicyAccounting, Stablecoin, StablecoinAccounting, StablecoinV1,
    StablecoinV2,
};

/// An activated version of the stablecoin B-20 precompile logic.
///
/// Each variant maps to an immutable implementation via [`Self::implementation`]. Variants are
/// declared in activation order, so the derived ordering is chronological.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StablecoinVersion {
    /// Introduced at Beryl, the stablecoin's activation fork.
    V1,
    /// Introduced at Cobalt.
    V2,
}

impl StablecoinVersion {
    /// Returns the immutable logic implementation for this version.
    pub fn implementation<'l, S, A>(self) -> &'l dyn Stablecoin<S, A>
    where
        S: StablecoinAccounting + 'l,
        A: PolicyAccounting + 'l,
    {
        static V1: StablecoinV1 = StablecoinV1;
        static V2: StablecoinV2 = StablecoinV2;
        match self {
            Self::V1 => &V1,
            Self::V2 => &V2,
        }
    }

    /// Returns the frozen common [`B20Abi`] wire surface this version decodes against.
    ///
    /// Gating the shared surface per version is what freezes V1/Beryl against selectors and enum
    /// members added to the canonical surface at a later fork (e.g. the Cobalt seize surface and its
    /// `SEIZE` `PausableFeature` member).
    pub const fn common_abi(self) -> B20Abi {
        match self {
            Self::V1 => B20Abi::V1,
            Self::V2 => B20Abi::V2,
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
    ///
    /// V1 is active from Beryl; V2 supersedes it from Cobalt.
    pub fn from_base_upgrade(upgrade: BaseUpgrade) -> Option<StablecoinVersion> {
        match upgrade {
            u if u >= BaseUpgrade::Cobalt => Some(StablecoinVersion::V2),
            u if u >= BaseUpgrade::Beryl => Some(StablecoinVersion::V1),
            _ => None,
        }
    }

    /// Grants `role` to `account` without checking caller authorization, using the stablecoin
    /// implementation active at `upgrade`.
    ///
    /// Parallel to [`Self::from_base_upgrade`] rather than built on it, since
    /// `grant_role_unchecked` is deliberately not part of the `Stablecoin` trait (see
    /// [`StablecoinV1::grant_role_unchecked`]) and so isn't reachable through
    /// [`StablecoinVersion::implementation`].
    pub fn grant_role_unchecked<S, A>(
        upgrade: BaseUpgrade,
        token: &mut B20StablecoinToken<S, A>,
        role: B256,
        account: Address,
        sender: Address,
    ) -> Result<()>
    where
        S: StablecoinAccounting,
        A: PolicyAccounting,
    {
        match upgrade {
            u if u >= BaseUpgrade::Cobalt => {
                StablecoinV2.grant_role_unchecked(token, role, account, sender)
            }
            u if u >= BaseUpgrade::Beryl => {
                StablecoinV1.grant_role_unchecked(token, role, account, sender)
            }
            // Unreachable in practice — the precompile is only installed from Beryl — but we
            // revert defensively, mirroring `dispatch_with_observer`.
            _ => Err(BasePrecompileError::Revert(Bytes::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::BaseUpgrade;

    use crate::{StablecoinVersion, StablecoinVersions};

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
    fn resolves_v2_at_cobalt() {
        assert_eq!(
            StablecoinVersions::from_base_upgrade(BaseUpgrade::Cobalt),
            Some(StablecoinVersion::V2)
        );
    }
}
