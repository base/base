//! Ethereum utilities for the host.

mod provider;
pub use provider::RpcProviderFactory;

mod precompiles;
pub use precompiles::{ACCELERATED_PRECOMPILES, execute};
