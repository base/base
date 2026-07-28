//! `B20StablecoinToken` native precompile — stablecoin variant of the B-20 token.

/// Rejects a call as an unknown selector, freezing the observable behavior of every version that
/// predates the selector.
///
/// V2-only selectors (the seize surface) live as **shared trait defaults** on [`Stablecoin`], so
/// each version inherits these bodies until it explicitly overrides them. A version introduced
/// *before* a selector existed must reject it exactly as it did at its activation fork; returning
/// `UnknownFunctionSelector` keeps re-executing a historical block on a newer binary
/// byte-identical to what the chain already committed.
///
/// The `[0u8; 4]` is a placeholder: the dispatcher rejects these selectors on the raw calldata,
/// before any version method is reached (see the fork gate in `route`), so a reachable call never
/// actually returns this value. Pinned by `golden_v2_selectors_unknown_at_v1`.
macro_rules! reject_frozen_selector {
    () => {
        ::core::result::Result::Err(
            ::base_precompile_storage::BasePrecompileError::UnknownFunctionSelector([0u8; 4]),
        )
    };
}

mod abi;
pub use abi::IB20Stablecoin;

mod accounting;
pub use accounting::StablecoinAccounting;

mod dispatch;

mod versions;
pub use versions::{StablecoinVersion, StablecoinVersions};

mod logic;
pub use logic::{B20StablecoinToken, Stablecoin, StablecoinV1, StablecoinV2};

mod precompile;
pub use precompile::B20StablecoinPrecompile;

mod storage;
pub use storage::{B20StablecoinExtensionStorage, B20StablecoinInit, B20StablecoinStorage};
