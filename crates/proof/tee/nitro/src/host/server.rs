use std::{fmt, net::SocketAddr, sync::Arc, time::Duration};

use base_health::{HealthzApiServer, HealthzRpc};
use base_proof_host::{ProverConfig, ProverService};
use base_proof_primitives::{EnclaveApiServer, ProofRequest, ProofResult, ProverApiServer};
use jsonrpsee::{
    RpcModule,
    core::{RpcResult, async_trait},
    server::{Server, ServerHandle, middleware::http::ProxyGetRequestLayer},
};
use tracing::info;

use super::{NitroBackend, transport::NitroTransport};

/// Host-side TEE prover server exposing a JSON-RPC interface.
///
/// Implements two JSON-RPC namespaces:
/// - `prover_*`: proving operations (forwarded to the enclave via transport)
/// - `enclave_*`: signer info queries (also forwarded via transport)
pub struct NitroProverServer {
    service: ProverService<NitroBackend>,
    transport: Arc<NitroTransport>,
}

impl fmt::Debug for NitroProverServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NitroProverServer").finish_non_exhaustive()
    }
}

impl NitroProverServer {
    /// Create a server with the given prover config and enclave transport.
    pub fn new(config: ProverConfig, transport: Arc<NitroTransport>) -> Self {
        let backend = NitroBackend::new(Arc::clone(&transport));
        Self { service: ProverService::new(config, backend), transport }
    }

    /// Start the JSON-RPC HTTP server on the given address.
    pub async fn run(self, addr: SocketAddr) -> eyre::Result<ServerHandle> {
        let middleware = tower::ServiceBuilder::new()
            .layer(
                ProxyGetRequestLayer::new([("/healthz", "healthz")])
                    .expect("valid healthz proxy layer"),
            )
            .timeout(Duration::from_secs(2));
        let server = Server::builder().set_http_middleware(middleware).build(addr).await?;
        let addr = server.local_addr()?;
        info!(addr = %addr, "nitro rpc server started");

        let mut module = RpcModule::new(());
        module.merge(NitroProverRpc { service: self.service }.into_rpc())?;
        module.merge(NitroSignerRpc { transport: self.transport }.into_rpc())?;
        module.merge(HealthzRpc::new(env!("CARGO_PKG_VERSION")).into_rpc())?;

        Ok(server.start(module))
    }
}

/// Inner RPC handler for `prover_*` methods.
struct NitroProverRpc {
    service: ProverService<NitroBackend>,
}

#[async_trait]
impl ProverApiServer for NitroProverRpc {
    async fn prove(&self, request: ProofRequest) -> RpcResult<ProofResult> {
        self.service.prove_block(request).await.map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(-32000, e.to_string(), None::<()>)
        })
    }
}

/// Inner RPC handler for `enclave_*` methods.
struct NitroSignerRpc {
    transport: Arc<NitroTransport>,
}

#[async_trait]
impl EnclaveApiServer for NitroSignerRpc {
    async fn signer_public_key(&self) -> RpcResult<Vec<u8>> {
        self.transport.signer_public_key().await.map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(-32001, e.to_string(), None::<()>)
        })
    }

    async fn signer_attestation(&self) -> RpcResult<Vec<u8>> {
        self.transport.signer_attestation().await.map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(-32001, e.to_string(), None::<()>)
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use base_proof_primitives::EnclaveApiServer;

    use super::*;
    use crate::enclave::{EnclaveConfig, Server as EnclaveServer};

    fn test_config() -> EnclaveConfig {
        EnclaveConfig {
            vsock_cid: 0,
            vsock_port: 0,
            config_hash: B256::ZERO,
            tee_image_hash: B256::ZERO,
        }
    }

    #[tokio::test]
    async fn signer_public_key_routed_to_transport() {
        let config = test_config();
        let server = Arc::new(EnclaveServer::new(&config).unwrap());
        let transport = Arc::new(NitroTransport::local(Arc::clone(&server)));
        let expected = server.signer_public_key();

        let rpc = NitroSignerRpc { transport };
        let result = EnclaveApiServer::signer_public_key(&rpc).await.unwrap();
        assert_eq!(result, expected);
        assert_eq!(result.len(), 65);
        assert_eq!(result[0], 0x04);
    }

    #[tokio::test]
    async fn healthz_returns_version() {
        let rpc = HealthzRpc::new(env!("CARGO_PKG_VERSION"));
        let result = HealthzApiServer::healthz(&rpc).await.unwrap();
        assert_eq!(result.version, env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn signer_attestation_routed_to_transport() {
        let config = test_config();
        let server = Arc::new(EnclaveServer::new(&config).unwrap());
        let transport = Arc::new(NitroTransport::local(Arc::clone(&server)));

        let rpc = NitroSignerRpc { transport };
        // NSM is unavailable outside a real Nitro enclave, so attestation fails.
        // Assert the error is propagated (not swallowed) through the RPC layer.
        let result = EnclaveApiServer::signer_attestation(&rpc).await;
        assert!(result.is_err());
    }
}
