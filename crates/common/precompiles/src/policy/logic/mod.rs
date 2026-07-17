//! Versioned business logic for the `PolicyRegistry` precompile.
//!
//! [`PolicyRegistryLogic`] (in [`interface`](self)) is the append-only business-logic
//! interface each version implements; [`PolicyRegistryLogicV1`] is the first frozen
//! implementation. PolicyRegistryLogic methods take a [`crate::policy::ContractContext`]
//! wrapping the storage port.

mod interface;
pub use interface::PolicyRegistryLogic;

mod v1;
pub use v1::PolicyRegistryLogicV1;
