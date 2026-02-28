//! Prover configuration.

use crate::transport::TransportConfig;

/// Top-level prover configuration.
///
/// Selects between running the Nitro enclave server or the HTTP-to-vsock proxy.
#[derive(Debug, Clone)]
pub enum ProverConfig {
    /// Run the Nitro enclave server with the given transport configuration.
    Nitro(TransportConfig),
    /// Run the HTTP-to-vsock proxy.
    Proxy {
        /// Vsock CID of the enclave.
        vsock_cid: u32,
        /// Vsock port to connect to.
        vsock_port: u32,
        /// HTTP port to listen on.
        http_port: u16,
    },
}

impl ProverConfig {
    /// Run the prover with this configuration.
    pub async fn run(self) -> eyre::Result<()> {
        match self {
            Self::Nitro(config) => crate::run_enclave(config).await,
            Self::Proxy { vsock_cid, vsock_port, http_port } => {
                crate::run_proxy(vsock_cid, vsock_port, http_port).await
            }
        }
    }
}
