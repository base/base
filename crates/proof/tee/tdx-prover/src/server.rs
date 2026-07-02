use std::{net::SocketAddr, sync::Arc};

use base_health::{HealthzApiServer, HealthzRpc};
use base_proof_primitives::EnclaveApiServer;
use base_proof_tee_tdx_runtime::TdxRuntime;
use jsonrpsee::{
    RpcModule,
    core::{RpcResult, async_trait},
    server::{Server, ServerHandle, middleware::http::ProxyGetRequestLayer},
};
use tracing::info;

use crate::TdxSignerAttestation;

/// Registrar-facing TDX prover server exposing health and signer JSON-RPC methods.
#[derive(Debug)]
pub struct TdxProverServer {
    runtime: Arc<TdxRuntime>,
}

impl TdxProverServer {
    /// Create a registrar-facing server for one TDX runtime.
    pub fn new(runtime: Arc<TdxRuntime>) -> Self {
        Self { runtime }
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
        module.merge(self.into_rpc())?;

        Ok(module)
    }
}

#[async_trait]
impl EnclaveApiServer for TdxProverServer {
    async fn signer_public_key(&self) -> RpcResult<Vec<Vec<u8>>> {
        Ok(vec![self.runtime.signer_public_key().to_vec()])
    }

    async fn signer_attestation(
        &self,
        user_data: Option<Vec<u8>>,
        nonces: Option<Vec<Vec<u8>>>,
    ) -> RpcResult<Vec<Vec<u8>>> {
        if user_data.is_some() || nonces.is_some() {
            return Err(jsonrpsee::types::ErrorObjectOwned::owned(
                -32602,
                "TDX signer attestations do not support user_data or nonce challenge binding",
                None::<()>,
            ));
        }

        let signer_public_key = self.runtime.signer_public_key();
        let quote = self.runtime.signer_quote().map_err(|error| {
            jsonrpsee::types::ErrorObjectOwned::owned(-32001, error.to_string(), None::<()>)
        })?;
        Ok(vec![
            TdxSignerAttestation {
                signer_public_key: signer_public_key.to_vec().into(),
                quote: quote.quote,
                quote_timestamp_millis: quote.quote_timestamp_millis,
            }
            .encode(),
        ])
    }

    async fn attestation_kind(&self) -> RpcResult<String> {
        Ok("tdx".to_owned())
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

    fn test_rpc() -> TdxProverServer {
        TdxProverServer::new(test_runtime())
    }

    #[tokio::test]
    async fn signer_attestation_serves_self_contained_tdx_payload() {
        let rpc = test_rpc();
        let result = EnclaveApiServer::signer_attestation(&rpc, None, None).await.unwrap();

        assert_eq!(result.len(), 1);
        let attestation = TdxSignerAttestation::decode(&result[0]).unwrap();
        let quote = base_proof_tee_tdx_verifier::TdxQuote::parse(&attestation.quote).unwrap();
        assert_eq!(attestation.signer_public_key, rpc.runtime.signer_public_key().to_vec());
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
    async fn local_mock_server_serves_json_rpc_methods() {
        let module = TdxProverServer::new(test_runtime()).into_rpc_module().unwrap();
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

        assert_eq!(kind, "tdx");
        assert_eq!(public_keys.len(), 1);
        assert_eq!(public_keys[0].len(), 65);
        assert_eq!(attestations.len(), 1);
        let attestation = TdxSignerAttestation::decode(&attestations[0]).unwrap();
        assert_eq!(attestation.signer_public_key, public_keys[0]);
        assert!(base_proof_tee_tdx_verifier::TdxQuote::parse(&attestation.quote).is_ok());
        let err = proof_result.unwrap_err();
        assert!(err.to_string().contains("Method not found"));
    }
}
