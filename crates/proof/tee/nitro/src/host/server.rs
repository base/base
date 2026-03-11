use std::{fmt, net::SocketAddr, sync::Arc};

use base_proof_host::{ProverConfig, ProverService};
use base_proof_primitives::{EnclaveApiServer, ProofRequest, ProofResult, ProverApiServer};
use base_proof_transport::ProofTransport;
use jsonrpsee::{
    RpcModule,
    core::{RpcResult, async_trait},
    server::{Server, ServerHandle},
};
use tracing::info;

use super::NitroBackend;

/// Host-side TEE prover server exposing a JSON-RPC interface.
///
/// Implements two JSON-RPC namespaces:
/// - `prover_*`: proving operations (forwarded to the enclave via transport)
/// - `enclave_*`: signer info queries (also forwarded via transport)
pub struct NitroProverServer {
    service: ProverService<NitroBackend>,
    transport: Arc<dyn ProofTransport>,
}

impl fmt::Debug for NitroProverServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NitroProverServer").finish_non_exhaustive()
    }
}

impl NitroProverServer {
    /// Create a server with the given prover config and enclave transport.
    pub fn new(config: ProverConfig, transport: Arc<dyn ProofTransport>) -> Self {
        let backend = NitroBackend::new(Arc::clone(&transport));
        Self { service: ProverService::new(config, backend), transport }
    }

    /// Start the JSON-RPC HTTP server on the given address.
    pub async fn run(self, addr: SocketAddr) -> eyre::Result<ServerHandle> {
        let server = Server::builder().build(addr).await?;
        let addr = server.local_addr()?;
        info!(addr = %addr, "nitro rpc server started");

        let mut module = RpcModule::new(());
        module.merge(NitroProverRpc { service: self.service }.into_rpc())?;
        module.merge(NitroSignerRpc { transport: self.transport }.into_rpc())?;

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
    transport: Arc<dyn ProofTransport>,
}

#[async_trait]
impl EnclaveApiServer for NitroSignerRpc {
    async fn signer_public_key(&self) -> RpcResult<Vec<u8>> {
        self.transport.signer_public_key().await.map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(-32001, e.to_string(), None::<()>)
        })
    }
}

#[cfg(test)]
mod tests {
    use base_proof_preimage::PreimageKey;
    use base_proof_primitives::ProofResult;
    use base_proof_transport::{NativeTransport, TransportError, TransportResult};

    use super::*;

    struct MockSignerTransport {
        public_key: Vec<u8>,
    }

    #[async_trait]
    impl ProofTransport for MockSignerTransport {
        async fn prove(
            &self,
            _preimages: &[(PreimageKey, Vec<u8>)],
        ) -> TransportResult<ProofResult> {
            unimplemented!("not used in signer tests")
        }

        async fn signer_public_key(&self) -> TransportResult<Vec<u8>> {
            Ok(self.public_key.clone())
        }
    }

    #[tokio::test]
    async fn signer_public_key_unsupported_for_native_transport() {
        let transport: NativeTransport<fn(&_) -> _> = NativeTransport::new(|_| unimplemented!());
        let result = transport.signer_public_key().await;
        assert!(matches!(result, Err(TransportError::Unsupported(_))));
    }

    #[tokio::test]
    async fn signer_public_key_routed_to_transport() {
        let expected = vec![0x04u8; 65];
        let rpc = NitroSignerRpc {
            transport: Arc::new(MockSignerTransport { public_key: expected.clone() }),
        };
        let result = EnclaveApiServer::signer_public_key(&rpc).await.unwrap();
        assert_eq!(result, expected);
    }
}
