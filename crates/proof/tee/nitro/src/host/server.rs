use std::{fmt, net::SocketAddr, sync::Arc};

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

/// Maximum allowed size for the `user_data` attestation field (NSM limit).
const MAX_USER_DATA_BYTES: usize = 512;

/// Maximum allowed size for the `nonce` attestation field (NSM limit).
const MAX_NONCE_BYTES: usize = 512;

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
            .layer(ProxyGetRequestLayer::new([("/healthz", "healthz")])?);
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

    async fn signer_attestation(
        &self,
        user_data: Option<Vec<u8>>,
        nonce: Option<Vec<u8>>,
    ) -> RpcResult<Vec<u8>> {
        // NSM limits: user_data ≤ 512 bytes, nonce ≤ 512 bytes.
        // Reject oversized payloads early to avoid allocating and forwarding them
        // through the vsock transport only to be rejected by the enclave.
        if user_data.as_ref().is_some_and(|d| d.len() > MAX_USER_DATA_BYTES) {
            return Err(jsonrpsee::types::ErrorObjectOwned::owned(
                -32602,
                format!("user_data exceeds {MAX_USER_DATA_BYTES}-byte limit"),
                None::<()>,
            ));
        }
        if nonce.as_ref().is_some_and(|n| n.len() > MAX_NONCE_BYTES) {
            return Err(jsonrpsee::types::ErrorObjectOwned::owned(
                -32602,
                format!("nonce exceeds {MAX_NONCE_BYTES}-byte limit"),
                None::<()>,
            ));
        }

        self.transport.signer_attestation(user_data, nonce).await.map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(-32001, e.to_string(), None::<()>)
        })
    }
}

#[cfg(test)]
mod tests {
    use base_proof_primitives::EnclaveApiServer;

    use super::*;
    use crate::enclave::Server as EnclaveServer;

    #[tokio::test]
    async fn signer_public_key_routed_to_transport() {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
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
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(Arc::clone(&server)));

        let rpc = NitroSignerRpc { transport };
        // NSM is unavailable outside a real Nitro enclave, so attestation fails.
        // Assert the error is propagated (not swallowed) through the RPC layer.
        let result = EnclaveApiServer::signer_attestation(&rpc, None, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn signer_attestation_rejects_oversized_user_data() {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(Arc::clone(&server)));
        let rpc = NitroSignerRpc { transport };

        let oversized = vec![0u8; MAX_USER_DATA_BYTES + 1];
        let result = EnclaveApiServer::signer_attestation(&rpc, Some(oversized), None).await;
        let err = result.unwrap_err();
        assert_eq!(err.code(), -32602);
        assert!(err.message().contains("user_data"));
    }

    #[tokio::test]
    async fn signer_attestation_rejects_oversized_nonce() {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(Arc::clone(&server)));
        let rpc = NitroSignerRpc { transport };

        let oversized = vec![0u8; MAX_NONCE_BYTES + 1];
        let result = EnclaveApiServer::signer_attestation(&rpc, None, Some(oversized)).await;
        let err = result.unwrap_err();
        assert_eq!(err.code(), -32602);
        assert!(err.message().contains("nonce"));
    }
}
