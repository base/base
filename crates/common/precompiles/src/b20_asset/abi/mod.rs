//! Wire (ABI) surfaces for the asset B-20 precompile, one per hardfork that moved them.
//!
//! The latest surface is always named `IB20Asset` in its `vN` module, then re-exported here as
//! both [`IB20Asset`] (canonical) and `IB20AssetVN`. Older forks keep the same Rust name inside
//! their module so truncated-calldata revert bytes stay stable, and are re-exported as
//! [`IB20AssetV1`], [`IB20AssetV2`], etc.
//!
//! Only the asset-specific surface is versioned here. The inherited common surface lives under
//! [`crate::B20Abi`] and is joined with this extension by [`crate::AssetVersion::abi`].
//!
//! This module is pure glue: surface definitions, the `as_label` mapping, the ERC-165 ids, and all
//! tests live in the `vN` modules; the canonical (newest) surface owns anything keyed to it.

mod v1;
pub use v1::IB20Asset as IB20AssetV1;

mod v2;
pub use v2::{ERC165_INTERFACE_ID, ERC8056_INTERFACE_IDS, IB20Asset, IB20Asset as IB20AssetV2};
