//! Native precompiles for Base-native tokens (B-20).

pub mod abi;
pub mod default_token;
pub use default_token::{DefaultToken, DEFAULT_TOKEN_ADDRESS, dispatch};
