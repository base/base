use std::{fmt, net::SocketAddr, sync::Arc, time::Duration};

use base_health::{HealthzApiServer, HealthzRpc};
use base_proof_contracts::TEEProverRegistryContractClient;
use base_proof_host::{ProverConfig, ProverService};
use base_proof_primitives::{EnclaveApiServer, ProofRequest, ProofResult, ProverApiServer};
use jsonrpsee::{
    RpcModule,
    core::{RpcResult, async_trait},
    server::{Server, ServerHandle, middleware::http::ProxyGetRequestLayer},
};
use tracing::{info, warn};

use super::{
    NitroBackend,
    health::{RegistrationHealthConfig, RegistrationHealthzRpc},
    registration::RegistrationChecker,
    transport::NitroTransport,
};

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
    proof_request_timeout: Duration,
    registration_health: Option<RegistrationHealthConfig>,
}

impl fmt::Debug for NitroProverServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NitroProverServer").finish_non_exhaustive()
    }
}

impl NitroProverServer {
    /// Create a server with the given prover config, enclave transport, and proof request timeout.
    pub fn new(
        config: ProverConfig,
        transport: Arc<NitroTransport>,
        proof_request_timeout: Duration,
    ) -> Self {
        let backend = NitroBackend::new(Arc::clone(&transport));
        Self {
            service: ProverService::new(config, backend),
            transport,
            proof_request_timeout,
            registration_health: None,
        }
    }

    /// Enables registration-gated health checks. When set, `/healthz` verifies
    /// the enclave signer is registered in the `TEEProverRegistry` on L1.
    pub fn with_registration_health(mut self, config: RegistrationHealthConfig) -> Self {
        self.registration_health = Some(config);
        self
    }

    /// Start the JSON-RPC HTTP server on the given address.
    pub async fn run(self, addr: SocketAddr) -> eyre::Result<ServerHandle> {
        let middleware = tower::ServiceBuilder::new()
            .layer(ProxyGetRequestLayer::new([("/healthz", "healthz")])?);
        let server = Server::builder().set_http_middleware(middleware).build(addr).await?;
        let addr = server.local_addr()?;
        info!(addr = %addr, "nitro rpc server started");

        let mut module = RpcModule::new(());

        let checker = match self.registration_health {
            Some(config) => {
                info!(
                    registry = %config.registry_address,
                    "registration-gated health and proving guard enabled"
                );
                let l1_url = url::Url::parse(&config.l1_rpc_url)
                    .map_err(|e| eyre::eyre!("invalid L1 RPC URL: {e}"))?;
                let registry =
                    TEEProverRegistryContractClient::new(config.registry_address, l1_url);
                let checker = Arc::new(
                    RegistrationChecker::new(vec![Arc::clone(&self.transport)], registry)
                        .map_err(|e| eyre::eyre!("registration checker init failed: {e}"))?,
                );
                module.merge(
                    RegistrationHealthzRpc::new(env!("CARGO_PKG_VERSION"), Arc::clone(&checker))
                        .into_rpc(),
                )?;
                Some(checker)
            }
            None => {
                module.merge(HealthzRpc::new(env!("CARGO_PKG_VERSION")).into_rpc())?;
                None
            }
        };

        module.merge(
            NitroProverRpc {
                service: self.service,
                proof_request_timeout: self.proof_request_timeout,
                checker,
            }
            .into_rpc(),
        )?;

        module
            .merge(NitroSignerRpc { transports: vec![Arc::clone(&self.transport)] }.into_rpc())?;

        Ok(server.start(module))
    }
}

/// Inner RPC handler for `prover_*` methods.
struct NitroProverRpc {
    service: ProverService<NitroBackend>,
    proof_request_timeout: Duration,
    checker: Option<Arc<RegistrationChecker>>,
}

#[async_trait]
impl ProverApiServer for NitroProverRpc {
    async fn prove(&self, request: ProofRequest) -> RpcResult<ProofResult> {
        if let Some(checker) = &self.checker {
            checker.require_valid_signer().await.map_err(|e| {
                warn!(error = %e, "rejecting proof request: signer validation failed");
                jsonrpsee::types::ErrorObjectOwned::owned(-32001, e.to_string(), None::<()>)
            })?;
        }

        let l2_block = request.claimed_l2_block_number;
        let timeout = self.proof_request_timeout;

        match tokio::time::timeout(timeout, self.service.prove_block(request)).await {
            Ok(result) => result.map_err(|e| {
                jsonrpsee::types::ErrorObjectOwned::owned(-32000, e.to_string(), None::<()>)
            }),
            Err(_elapsed) => {
                warn!(l2_block, timeout_secs = timeout.as_secs(), "proof request timed out");
                Err(jsonrpsee::types::ErrorObjectOwned::owned(
                    -32000,
                    format!(
                        "proof request timed out after {}s for L2 block {l2_block}",
                        timeout.as_secs()
                    ),
                    None::<()>,
                ))
            }
        }
    }
}

/// Inner RPC handler for `enclave_*` methods.
///
/// All-or-nothing: both `signer_public_key` and `signer_attestation` fail if
/// **any** transport is unreachable.  Callers need the complete set of keys /
/// attestations (one per enclave) to register all signers on-chain, so a
/// partial response would be unusable.
struct NitroSignerRpc {
    transports: Vec<Arc<NitroTransport>>,
}

#[async_trait]
impl EnclaveApiServer for NitroSignerRpc {
    async fn signer_public_key(&self) -> RpcResult<Vec<Vec<u8>>> {
        let mut keys = Vec::with_capacity(self.transports.len());
        for transport in &self.transports {
            let key = transport.signer_public_key().await.map_err(|e| {
                jsonrpsee::types::ErrorObjectOwned::owned(-32001, e.to_string(), None::<()>)
            })?;
            keys.push(key);
        }
        Ok(keys)
    }

    async fn signer_attestation(
        &self,
        user_data: Option<Vec<u8>>,
        nonce: Option<Vec<u8>>,
    ) -> RpcResult<Vec<Vec<u8>>> {
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

        let mut attestations = Vec::with_capacity(self.transports.len());
        for transport in &self.transports {
            let attestation = transport
                .signer_attestation(user_data.clone(), nonce.clone())
                .await
                .map_err(|e| {
                    jsonrpsee::types::ErrorObjectOwned::owned(-32001, e.to_string(), None::<()>)
                })?;
            attestations.push(attestation);
        }
        Ok(attestations)
    }
}

#[cfg(test)]
mod tests {
    use base_proof_primitives::EnclaveApiServer;
    use base_proof_tee_nitro_enclave::Server as EnclaveServer;

    use super::*;

    #[tokio::test]
    async fn signer_public_key_routed_to_transport() {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(Arc::clone(&server)));
        let expected = server.signer_public_key();

        let rpc = NitroSignerRpc { transports: vec![transport] };
        let result = EnclaveApiServer::signer_public_key(&rpc).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], expected);
        assert_eq!(result[0].len(), 65);
        assert_eq!(result[0][0], 0x04);
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

        let rpc = NitroSignerRpc { transports: vec![transport] };
        // NSM is unavailable outside a real Nitro enclave, so attestation fails.
        // Assert the error is propagated (not swallowed) through the RPC layer.
        let result = EnclaveApiServer::signer_attestation(&rpc, None, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn signer_attestation_rejects_oversized_user_data() {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(Arc::clone(&server)));
        let rpc = NitroSignerRpc { transports: vec![transport] };

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
        let rpc = NitroSignerRpc { transports: vec![transport] };

        let oversized = vec![0u8; MAX_NONCE_BYTES + 1];
        let result = EnclaveApiServer::signer_attestation(&rpc, None, Some(oversized)).await;
        let err = result.unwrap_err();
        assert_eq!(err.code(), -32602);
        assert!(err.message().contains("nonce"));
    }
}
