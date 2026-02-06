//! JSON-RPC server module for the enclave.
//!
//! This module provides the RPC interface for the enclave server, matching
//! the Go implementation in `enclave/rpc.go`.

mod api;
mod server;
mod types;

pub use api::{EnclaveApiClient, EnclaveApiServer};
pub use server::RpcServerImpl;
pub use types::{AggregateRequest, ExecuteStatelessRequest};
