//! Self-contained per-version implementations of the `foo` precompile.
//!
//! Unlike a shared per-method logic trait, each version here implements the
//! whole [`FooVersion::call`](crate::FooVersion) seam itself: it decodes and
//! routes its own selectors and holds its own logic. Storage layout and the ABI
//! stay shared (they are cross-version invariants); routing and behavior do not.

mod v1;
pub use v1::FooV1;

mod v2;
pub use v2::FooV2;
