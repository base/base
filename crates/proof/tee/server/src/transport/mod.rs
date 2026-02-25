//! Transport configuration for the enclave server.
//!
//! Supports vsock (for Nitro Enclaves) and HTTP (for development/testing).

pub mod config;

pub use config::{
    DEFAULT_HTTP_BODY_LIMIT, DEFAULT_HTTP_PORT, DEFAULT_PROXY_PORT, DEFAULT_VSOCK_CID,
    DEFAULT_VSOCK_PORT, HTTP_BODY_LIMIT_ENV, TransportConfig,
};
