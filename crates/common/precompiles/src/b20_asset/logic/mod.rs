//! Self-contained per-version implementations of the `b20_asset` precompile.
//!
//! Each version owns the whole [`B20AssetVersion`](crate::B20AssetVersion) seam:
//! it decodes and routes its own selectors and holds its own business logic.
//! Storage layout and the ABI stay shared (cross-version invariants); routing
//! and behavior do not.

mod v1;
pub use v1::B20AssetV1;
