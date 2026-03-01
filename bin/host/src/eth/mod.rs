//! Ethereum utilities for the host binary.

pub use base_proof_host::rpc_provider;

mod precompiles;
pub(crate) use precompiles::execute;
