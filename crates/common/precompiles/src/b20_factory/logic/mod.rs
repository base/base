//! Versioned business logic for the B-20 token factory precompile.

mod interface;
pub use interface::Factory;

mod v1;
pub use v1::{CommonParams, FactoryV1, TokenCreateParams};
