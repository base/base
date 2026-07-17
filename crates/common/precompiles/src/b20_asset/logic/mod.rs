//! Versioned business logic for the asset B-20 precompile.
//!
//! [`Logic`] (in [`interface`](self)) is the append-only business-logic
//! interface each version implements; [`LogicV1`] is the first frozen
//! implementation. The storage + policy bag they operate on is
//! [`crate::ContractContext`].

mod interface;
pub use interface::Logic;

mod v1;
pub use v1::LogicV1;
