//! Versioned business logic for the B-20 token factory precompile.
//!
//! [`Logic`] (in [`interface`](self)) is the append-only business-logic
//! interface each version implements; [`LogicV1`] is the first frozen
//! implementation.

mod interface;
pub use interface::Logic;

mod v1;
pub use v1::{CommonParams, LogicV1, TokenCreateParams};
