//! Versioned business logic for the `PolicyRegistry` precompile.
//!
//! [`PolicyRegistryLogic`] (in [`interface`](self)) is the append-only business-logic
//! interface each version implements; [`PolicyRegistryV1`] is the first frozen
//! implementation. Logic methods take a [`crate::PolicyAccounting`] storage port
//! directly — there is no separate runtime wrapper.

mod interface;
pub use interface::PolicyRegistryLogic;

mod v1;
pub use v1::PolicyRegistryV1;
