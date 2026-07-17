//! Versioned business logic for the asset B-20 precompile.
//!
//! [`B20AssetLogic`] (in [`interface`](self)) is the append-only business-logic
//! interface each version implements; [`B20AssetLogicV1`] is the first frozen
//! implementation. The storage + policy bag they operate on is
//! [`crate::ContractContext`].

mod interface;
pub use interface::B20AssetLogic;

mod v1;
pub use v1::B20AssetLogicV1;
