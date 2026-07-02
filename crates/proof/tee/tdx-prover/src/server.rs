use std::{fmt, net::SocketAddr, sync::Arc};

use base_health::{HealthzApiServer, HealthzRpc};
use base_proof_host::{ProverConfig, ProverService};
use base_proof_primitives::EnclaveApiServer;
use base_proof_tee_tdx_runtime::TdxRuntime;
use jsonrpsee::{
    RpcModule,
    core::{RpcResult, async_trait},
    server::{Server, ServerHandle, middleware::http::ProxyGetRequestLayer},
};
use tracing::info;

use crate::{TdxBackend, TdxSignerAttestation};

/// JSON-RPC attestation kind returned by TDX prover servers.
pub const TDX_ATTESTATION_KIND: &str = "tdx";

/// One TDX enclave runtime and its proving service.
pub struct TdxEnclaveService {
    service: ProverService<TdxBackend>,
}

impl TdxEnclaveService {
    /// Create a service wrapper for one TDX runtime.
    pub fn new(config: ProverConfig, runtime: Arc<TdxRuntime>) -> Self {
        let backend = TdxBackend::new(runtime);
        Self { service: ProverService::new(config, backend) }
    }

    /// Returns the prover service for this enclave.
    pub const fn service(&self) -> &ProverService<TdxBackend> {
        &self.service
    }
}

impl fmt::Debug for TdxEnclaveService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TdxEnclaveService").finish_non_exhaustive()
    }
}

/// Registrar-facing TDX prover server exposing health and signer JSON-RPC methods.
pub struct TdxProverServer {
    runtimes: Vec<Arc<TdxRuntime>>,
}

impl TdxProverServer {
    /// Convert an internal error into a JSON-RPC error object.
    pub fn rpc_err(code: i32, err: impl std::fmt::Display) -> jsonrpsee::types::ErrorObjectOwned {
        jsonrpsee::types::ErrorObjectOwned::owned(code, err.to_string(), None::<()>)
    }

    /// Create a registrar-facing server for one TDX runtime.
    pub fn new(runtime: Arc<TdxRuntime>) -> Self {
        Self::new_multi(vec![runtime])
    }

    /// Create a registrar-facing server for multiple TDX runtimes.
    ///
    /// # Panics
    ///
    /// Panics if `runtimes` is empty.
    pub fn new_multi(runtimes: Vec<Arc<TdxRuntime>>) -> Self {
        assert!(!runtimes.is_empty(), "at least one runtime is required");
        Self { runtimes }
    }

    /// Start the registrar-facing JSON-RPC HTTP server on the given address.
    pub async fn run(self, addr: SocketAddr) -> eyre::Result<ServerHandle> {
        let middleware = tower::ServiceBuilder::new()
            .layer(ProxyGetRequestLayer::new([("/healthz", "healthz")])?);
        let server = Server::builder().set_http_middleware(middleware).build(addr).await?;
        let addr = server.local_addr()?;
        info!(addr = %addr, "tdx registrar rpc server started");

        Ok(server.start(self.into_rpc_module()?))
    }

    /// Build the registrar-facing JSON-RPC module served by this TDX prover.
    pub fn into_rpc_module(self) -> eyre::Result<RpcModule<()>> {
        let mut module = RpcModule::new(());

        module.merge(HealthzRpc::new(env!("CARGO_PKG_VERSION")).into_rpc())?;
        module.merge(TdxSignerRpc::new(self.runtimes).into_rpc())?;

        Ok(module)
    }
}

impl fmt::Debug for TdxProverServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TdxProverServer").finish_non_exhaustive()
    }
}

/// Inner RPC handler for `enclave_*` methods.
pub struct TdxSignerRpc {
    /// TDX runtimes used for signer and quote collection calls.
    pub runtimes: Vec<Arc<TdxRuntime>>,
}

impl fmt::Debug for TdxSignerRpc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TdxSignerRpc").field("runtime_count", &self.runtimes.len()).finish()
    }
}

impl TdxSignerRpc {
    /// Create signer RPC over all available TDX runtimes.
    ///
    /// # Panics
    ///
    /// Panics if `runtimes` is empty.
    pub fn new(runtimes: Vec<Arc<TdxRuntime>>) -> Self {
        assert!(!runtimes.is_empty(), "at least one runtime is required");
        Self { runtimes }
    }
}

#[async_trait]
impl EnclaveApiServer for TdxSignerRpc {
    async fn signer_public_key(&self) -> RpcResult<Vec<Vec<u8>>> {
        Ok(self.runtimes.iter().map(|runtime| runtime.signer_public_key().to_vec()).collect())
    }

    async fn signer_attestation(
        &self,
        user_data: Option<Vec<u8>>,
        nonces: Option<Vec<Vec<u8>>>,
    ) -> RpcResult<Vec<Vec<u8>>> {
        if user_data.is_some() {
            return Err(TdxProverServer::rpc_err(
                -32602,
                "TDX signer attestations do not support user_data challenge binding",
            ));
        }
        if nonces.is_some() {
            return Err(TdxProverServer::rpc_err(
                -32602,
                "TDX signer attestations do not support nonce challenge binding",
            ));
        }

        let mut attestations = Vec::with_capacity(self.runtimes.len());
        for runtime in &self.runtimes {
            let signer_public_key = runtime.signer_public_key();
            let quote =
                runtime.signer_quote().map_err(|error| TdxProverServer::rpc_err(-32001, error))?;
            attestations.push(
                TdxSignerAttestation {
                    signer_public_key: signer_public_key.to_vec().into(),
                    quote: quote.quote,
                    quote_timestamp_millis: quote.quote_timestamp_millis,
                }
                .encode(),
            );
        }
        Ok(attestations)
    }

    async fn attestation_kind(&self) -> RpcResult<String> {
        Ok(TDX_ATTESTATION_KIND.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use base_proof_primitives::{EnclaveApiServer, ProofRequest, ProofResult};
    use jsonrpsee::{core::client::ClientT, http_client::HttpClientBuilder, rpc_params};

    use super::*;
    use crate::TdxMeasurements;

    fn test_runtime() -> Arc<TdxRuntime> {
        Arc::new(TdxRuntime::new(TdxMeasurements))
    }

    fn test_rpc() -> TdxSignerRpc {
        TdxSignerRpc::new(vec![test_runtime()])
    }

    fn multi_test_rpc() -> TdxSignerRpc {
        TdxSignerRpc::new(vec![test_runtime(), test_runtime()])
    }

    #[tokio::test]
    async fn signer_public_key_serves_tdx_signer_identity() {
        let rpc = test_rpc();
        let result = EnclaveApiServer::signer_public_key(&rpc).await.unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 65);
        assert_eq!(result[0][0], 0x04);
    }

    #[tokio::test]
    async fn signer_attestation_serves_self_contained_tdx_payload() {
        let rpc = test_rpc();
        let result = EnclaveApiServer::signer_attestation(&rpc, None, None).await.unwrap();

        assert_eq!(result.len(), 1);
        let attestation = TdxSignerAttestation::decode(&result[0]).unwrap();
        let quote = base_proof_tee_tdx_verifier::TdxQuote::parse(&attestation.quote).unwrap();
        assert_eq!(attestation.signer_public_key, rpc.runtimes[0].signer_public_key().to_vec());
        assert_eq!(
            quote.report_data_suffix(),
            base_proof_tee_tdx_verifier::TdxVerifier::timestamp_report_data_suffix(
                attestation.quote_timestamp_millis
            )
        );
    }

    #[tokio::test]
    async fn signer_attestation_rejects_user_data() {
        let rpc = test_rpc();
        let err = EnclaveApiServer::signer_attestation(&rpc, Some(vec![1, 2, 3]), None)
            .await
            .unwrap_err();

        assert_eq!(err.code(), -32602);
        assert!(err.message().contains("user_data"));
    }

    #[tokio::test]
    async fn signer_attestation_rejects_nonce() {
        let rpc = test_rpc();
        let err = EnclaveApiServer::signer_attestation(&rpc, None, Some(vec![vec![1, 2, 3]]))
            .await
            .unwrap_err();

        assert_eq!(err.code(), -32602);
        assert!(err.message().contains("nonce"));
    }

    #[tokio::test]
    async fn attestation_kind_serves_tdx() {
        let rpc = test_rpc();
        let result = EnclaveApiServer::attestation_kind(&rpc).await.unwrap();

        assert_eq!(result, TDX_ATTESTATION_KIND);
    }

    #[tokio::test]
    async fn signer_public_key_serves_all_tdx_signer_identities() {
        let rpc = multi_test_rpc();
        let result = EnclaveApiServer::signer_public_key(&rpc).await.unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0], rpc.runtimes[0].signer_public_key().to_vec());
        assert_eq!(result[1], rpc.runtimes[1].signer_public_key().to_vec());
        assert_ne!(result[0], result[1]);
    }

    #[tokio::test]
    async fn signer_attestation_serves_all_tdx_payloads() {
        let rpc = multi_test_rpc();
        let result = EnclaveApiServer::signer_attestation(&rpc, None, None).await.unwrap();

        assert_eq!(result.len(), 2);
        for (index, payload) in result.iter().enumerate() {
            let attestation = TdxSignerAttestation::decode(payload).unwrap();
            assert_eq!(
                attestation.signer_public_key,
                rpc.runtimes[index].signer_public_key().to_vec()
            );
            assert!(base_proof_tee_tdx_verifier::TdxQuote::parse(&attestation.quote).is_ok());
        }
    }

    #[tokio::test]
    async fn local_mock_server_serves_json_rpc_methods() {
        let signer_rpc = test_rpc();
        let mut module = RpcModule::new(());
        module.merge(HealthzRpc::new(env!("CARGO_PKG_VERSION")).into_rpc()).unwrap();
        module.merge(signer_rpc.into_rpc()).unwrap();
        let server =
            Server::builder().build("127.0.0.1:0".parse::<SocketAddr>().unwrap()).await.unwrap();
        let addr = server.local_addr().unwrap();
        let handle = server.start(module);
        let client = HttpClientBuilder::default().build(format!("http://{addr}")).unwrap();

        let kind: String = client.request("enclave_attestationKind", rpc_params![]).await.unwrap();
        let public_keys: Vec<Vec<u8>> =
            client.request("enclave_signerPublicKey", rpc_params![]).await.unwrap();
        let attestations: Vec<Vec<u8>> = client
            .request("enclave_signerAttestation", rpc_params![None::<Vec<u8>>, None::<Vec<u8>>])
            .await
            .unwrap();
        let proof_result = client
            .request::<ProofResult, _>("prover_prove", rpc_params![ProofRequest::default()])
            .await;

        handle.stop().unwrap();

        assert_eq!(kind, TDX_ATTESTATION_KIND);
        assert_eq!(public_keys.len(), 1);
        assert_eq!(public_keys[0].len(), 65);
        assert_eq!(attestations.len(), 1);
        let attestation = TdxSignerAttestation::decode(&attestations[0]).unwrap();
        assert_eq!(attestation.signer_public_key, public_keys[0]);
        assert!(base_proof_tee_tdx_verifier::TdxQuote::parse(&attestation.quote).is_ok());
        let err = proof_result.unwrap_err();
        assert!(err.to_string().contains("Method not found"));
    }

    #[tokio::test]
    async fn healthz_returns_version() {
        let rpc = HealthzRpc::new(env!("CARGO_PKG_VERSION"));
        let result = HealthzApiServer::healthz(&rpc).await.unwrap();
        assert_eq!(result.version, env!("CARGO_PKG_VERSION"));
    }
}
