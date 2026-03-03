//! Contract client modules for onchain dispute game interactions.
//!
//! Each sub-module defines a Solidity interface via [`alloy_sol_types::sol!`], an async client
//! trait, and a concrete Alloy-backed implementation.

mod aggregate_verifier;
pub use aggregate_verifier::*;

mod anchor_state_registry;
pub use anchor_state_registry::*;

mod dispute_game_factory;
pub use dispute_game_factory::*;
