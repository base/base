//! Versioned business logic for the B-20 token factory precompile.
//!
//! [`B20FactoryLogic`] (in [`interface`](self)) is the append-only business-logic
//! interface each version implements; [`B20FactoryLogicV1`] is the first frozen
//! implementation. Logic methods take a [`crate::FactoryContractContext`].

mod interface;
pub use interface::B20FactoryLogic;

mod v1;
pub use v1::{CommonParams, B20FactoryLogicV1, TokenCreateParams};
