//! Wire (ABI) surfaces for the shared B-20 token interface, one per hardfork that moved them.
//!
//! The latest surface is always named `IB20` in its `vN` module, then re-exported here as both
//! [`IB20`] (canonical) and `IB20VN`. Older forks keep the same Rust name inside their module so
//! truncated-calldata revert bytes stay stable, and are re-exported as [`IB20V1`], [`IB20V2`], etc.
//!
//! Token variants compose this surface with their own extension ABI. Asset does so via
//! [`crate::AssetAbiPair`]; stablecoin still decodes against canonical [`IB20`] directly until it
//! adopts the same composite shape.
//!
//! A fork that changes the common wire adds `abi/vN.rs` and retargets the canonical alias below.
//! Token versions then map onto the new [`B20Abi`] variant through their own `abi()` join — there
//! is no independent `B20Abi::from_base_upgrade`.

mod v1;
pub use v1::IB20 as IB20V1;

mod v2;
pub use v2::{IB20, IB20 as IB20V2};

mod b20_abi;
pub use b20_abi::B20Abi;
