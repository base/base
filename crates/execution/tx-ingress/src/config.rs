//! Transaction ingress server configuration.

use std::net::SocketAddr;

/// Configuration for the private transaction ingress server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransactionIngressConfig {
    /// Address on which to serve transaction ingress, or `None` to disable it.
    pub listen_addr: Option<SocketAddr>,
}

impl TransactionIngressConfig {
    /// Creates a configuration that listens on `listen_addr`.
    pub const fn new(listen_addr: SocketAddr) -> Self {
        Self { listen_addr: Some(listen_addr) }
    }
}
