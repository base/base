//! Versioned business logic for the stablecoin B-20 precompile.
//!
//! [`B20StablecoinLogic`] (in [`interface`](self)) is the append-only business-logic
//! interface each version implements; [`B20StablecoinLogicV1`] is the first frozen
//! implementation. The storage + policy bag they operate on is
//! [`super::ContractContext`].

mod interface;
pub use interface::B20StablecoinLogic;

mod v1;
pub use v1::B20StablecoinLogicV1;
