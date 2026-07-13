//! Fork→version resolution and the execution-logic seam for `b20_asset`.
//!
//! [`B20AssetVersion`] is the seam every version implements: a single
//! [`run`](B20AssetVersion::run) entry that performs selector routing plus the
//! business logic for that version (see [`crate::b20_asset::logic`]).
//! [`B20AssetVersions::resolve`] centralizes the fork→version mapping so
//! hardfork logic is auditable in one place and resolved once at install time.
//!
//! Unlike the `foo` reference precompile, B20 logic is generic over storage
//! (`S`), policy (`P`), and observer (`O`), so [`B20AssetVersion::run`] carries
//! those as generic type parameters. A generic method makes the trait
//! object-unsafe, so the resolver hands back a small `'static`, `Copy`
//! [`B20AssetVersionId`] enum that is dispatched statically, rather than a
//! `&'static dyn B20AssetVersion` trait object. (`foo` can use `&'static dyn`
//! only because its storage type is concrete and `'static`; B20's
//! [`B20AssetStorage`](crate::B20AssetStorage)`<'a>` borrows the call's
//! `StorageCtx`, so it is never `'static`.)

use alloy_primitives::Bytes;
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{Result, StorageCtx};

use crate::{AssetAccounting, B20AssetToken, B20AssetV1, Policy, PrecompileCallObserver};

/// The execution-logic seam every `b20_asset` version implements.
///
/// A version fully owns "which selectors exist and what they do":
/// [`run`](Self::run) is the relocated `inner_with_observer` routing. Storage
/// layout and the ABI stay shared across versions (they are cross-version
/// invariants); routing and behavior do not.
pub trait B20AssetVersion {
    /// Routes `calldata` to this version's selectors and executes the matching
    /// business logic against `token`, returning the ABI-encoded output (or an
    /// error/revert).
    ///
    /// Generic over storage (`S`), policy (`P`), and observer (`O`) because B20
    /// logic lives on the generic [`B20AssetToken`] and dispatch threads a
    /// generic observer.
    fn run<S, P, O>(
        &self,
        token: &mut B20AssetToken<S, P>,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        observer: O,
    ) -> Result<Bytes>
    where
        S: AssetAccounting,
        P: Policy,
        O: PrecompileCallObserver;
}

/// The `b20_asset` execution-logic version active at a given hardfork.
///
/// Resolved once at install time by [`B20AssetVersions::resolve`] and threaded
/// as a `'static`, `Copy` handle into the shared dispatch entry, which forwards
/// to the matching self-contained version via static dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum B20AssetVersionId {
    /// Logic active from Beryl onward — today's behavior, wrapped verbatim as V1.
    V1,
}

impl B20AssetVersionId {
    /// Runs the resolved version's selector routing and business logic against
    /// `token`, selecting the concrete [`B20AssetVersion`] implementation for
    /// this fork.
    pub fn run<S, P, O>(
        self,
        token: &mut B20AssetToken<S, P>,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        observer: O,
    ) -> Result<Bytes>
    where
        S: AssetAccounting,
        P: Policy,
        O: PrecompileCallObserver,
    {
        match self {
            Self::V1 => B20AssetV1.run(token, ctx, calldata, observer),
        }
    }
}

/// Fork→version resolver for the `b20_asset` precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct B20AssetVersions;

impl B20AssetVersions {
    /// Returns the version active at `upgrade`, or `None` before Beryl (where the
    /// B-20 precompiles are not installed at all).
    pub fn resolve(upgrade: BaseUpgrade) -> Option<B20AssetVersionId> {
        (upgrade >= BaseUpgrade::Beryl).then_some(B20AssetVersionId::V1)
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::BaseUpgrade;

    use crate::{B20AssetVersionId, B20AssetVersions};

    #[test]
    fn resolves_none_before_beryl() {
        assert!(B20AssetVersions::resolve(BaseUpgrade::Azul).is_none());
    }

    #[test]
    fn resolves_v1_from_beryl() {
        assert_eq!(B20AssetVersions::resolve(BaseUpgrade::Beryl), Some(B20AssetVersionId::V1));
    }

    #[test]
    fn resolves_v1_from_cobalt() {
        assert_eq!(B20AssetVersions::resolve(BaseUpgrade::Cobalt), Some(B20AssetVersionId::V1));
    }
}
