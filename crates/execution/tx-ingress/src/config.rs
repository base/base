//! Transaction ingress server configuration.

use std::net::SocketAddr;

/// Configuration for the private transaction ingress server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionIngressConfig {
    /// Address on which to serve transaction ingress.
    pub listen_addr: SocketAddr,
}

impl TransactionIngressConfig {
    /// Creates a configuration that listens on `listen_addr`.
    pub const fn new(listen_addr: SocketAddr) -> Self {
        Self { listen_addr }
    }
}
