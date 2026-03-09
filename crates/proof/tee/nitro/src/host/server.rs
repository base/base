use std::{net::SocketAddr, sync::Arc};

use base_proof_host::{ProverConfig, ProverService};
use base_proof_primitives::{ProofRequest, ProofResult, ProverApiServer};
use base_proof_transport::ProofTransport;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    server::{Server, ServerHandle},
};
use tracing::info;

use super::NitroBackend;

/// Host-side TEE prover server exposing a JSON-RPC interface.
#[derive(Debug)]
pub struct NitroProverServer {
    service: ProverService<NitroBackend>,
}

impl NitroProverServer {
    /// Create a server with the given prover config and enclave transport.
    pub fn new(config: ProverConfig, transport: Arc<dyn ProofTransport>) -> Self {
        let backend = NitroBackend::new(transport);
        Self { service: ProverService::new(config, backend) }
    }

    /// Start the JSON-RPC HTTP server on the given address.
    pub async fn run(self, addr: SocketAddr) -> eyre::Result<ServerHandle> {
        let server = Server::builder().build(addr).await?;
        let addr = server.local_addr()?;
        info!(addr = %addr, "nitro rpc server started");
        Ok(server.start(self.into_rpc()))
    }
}

#[async_trait]
impl ProverApiServer for NitroProverServer {
    async fn prove(&self, request: ProofRequest) -> RpcResult<ProofResult> {
        self.service.prove_block(request).await.map_err(|e| {
            jsonrpsee::types::ErrorObjectOwned::owned(-32000, e.to_string(), None::<()>)
        })
    }
}
