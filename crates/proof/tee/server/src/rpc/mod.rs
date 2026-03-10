//! JSON-RPC server module for the enclave.

mod api;
mod server;
mod types;

pub use api::{EnclaveApiClient, EnclaveApiServer};
pub use base_enclave::ExecuteStatelessRequest;
pub use server::RpcServerImpl;
pub use types::AggregateRequest;
