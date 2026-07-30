//! `B20AssetToken` native precompile — asset variant of the B-20 token.

/// Rejects a call as an unknown selector, freezing the observable behavior of every version that
/// predates the selector.
///
/// The ERC-8056 scheduled-multiplier surface lives as **shared trait defaults** on [`Asset`] and
/// [`AssetAccounting`], so each version inherits these bodies until it explicitly overrides them.
/// A version introduced *before* a selector existed must reject it exactly as it did at its
/// activation fork. Returning `UnknownFunctionSelector` rather than an `Ok` or a real value keeps
/// re-executing a historical block on a newer binary byte-identical to what the chain already
/// committed; any other default would retroactively change an older version's state transition, i.e.
/// a silent hardfork.
///
/// The `[0u8; 4]` is a placeholder: a version's frozen wire surface does not declare these
/// selectors, so `route` rejects them (via fallthrough to the disjoint inherited `IB20` decode)
/// before any version method is reached, and a reachable call never actually returns this value.
/// Pinned by `golden_v2_selectors_unknown_at_v1`.
macro_rules! reject_frozen_selector {
    () => {
        ::core::result::Result::Err(
            ::base_precompile_storage::BasePrecompileError::UnknownFunctionSelector([0u8; 4]),
        )
    };
}

mod abi;
pub use abi::{ERC165_INTERFACE_ID, ERC8056_INTERFACE_IDS, IB20Asset, IB20AssetV1, IB20AssetV2};

mod accounting;
pub use accounting::AssetAccounting;

mod dispatch;

mod versions;
pub use versions::{AssetAbi, AssetVersion, AssetVersions};

mod logic;
pub use logic::{Asset, AssetV1, AssetV2, B20AssetToken};

mod precompile;
pub use precompile::B20AssetPrecompile;

mod storage;
pub use storage::{B20AssetExtensionStorage, B20AssetInit, B20AssetStorage};
