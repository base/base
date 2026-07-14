use std::{net::SocketAddr, sync::Arc};

use alloy_primitives::B256;
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
    pub const fn new(runtime: Arc<TdxRuntime>) -> Self {
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
        if user_data.is_some() {
            return Err(jsonrpsee::types::ErrorObjectOwned::owned(
                -32602,
                "TDX signer attestations do not support user_data",
                None::<()>,
            ));
        }

        let attestation_nonce = match nonces {
            None => None,
            Some(nonces) => {
                let [nonce] = nonces.as_slice() else {
                    return Err(jsonrpsee::types::ErrorObjectOwned::owned(
                        -32602,
                        "TDX signer attestations require exactly one nonce",
                        None::<()>,
                    ));
                };
                let nonce: [u8; 32] = nonce.as_slice().try_into().map_err(|_| {
                    jsonrpsee::types::ErrorObjectOwned::owned(
                        -32602,
                        "TDX signer attestation nonce must be 32 bytes",
                        None::<()>,
                    )
                })?;
                Some(B256::from(nonce))
            }
        };

        let quote = self.runtime.signer_quote(attestation_nonce).map_err(|error| {
            jsonrpsee::types::ErrorObjectOwned::owned(-32001, error.to_string(), None::<()>)
        })?;
        Ok(vec![
            TdxSignerAttestation {
                signer_public_key: self.runtime.signer_public_key(),
                quote: quote.quote,
                quote_timestamp_millis: quote.quote_timestamp_millis,
                attestation_nonce,
            }
            .encode(),
        ])
    }
}

#[cfg(test)]
mod tests {
    use base_proof_primitives::EnclaveApiServer;

    use super::*;
    use crate::TdxMeasurements;

    fn test_rpc() -> TdxProverServer {
        TdxProverServer::new(Arc::new(TdxRuntime::new(TdxMeasurements)))
    }

    #[tokio::test]
    async fn signer_attestation_binds_registrar_nonce() {
        let rpc = test_rpc();
        let nonce = vec![0x11; 32];
        let result = EnclaveApiServer::signer_attestation(&rpc, None, Some(vec![nonce.clone()]))
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let attestation = TdxSignerAttestation::decode(&result[0]).unwrap();
        let quote = base_proof_tee_tdx_verifier::TdxQuote::parse(&attestation.quote).unwrap();
        assert_eq!(attestation.signer_public_key, rpc.runtime.signer_public_key().to_vec());
        assert_eq!(attestation.attestation_nonce, Some(B256::from([0x11; 32])));
        assert_eq!(
            quote.report_data_prefix(),
            base_proof_tee_tdx_verifier::TdxVerifier::validate_public_key(
                &attestation.signer_public_key
            )
            .unwrap()
        );
        assert_eq!(
            quote.report_data_suffix(),
            base_proof_tee_tdx_verifier::TdxVerifier::timestamp_report_data_suffix(
                attestation.quote_timestamp_millis,
                attestation.attestation_nonce
            )
        );
    }

    #[tokio::test]
    async fn signer_attestation_rejects_user_data_and_invalid_nonces() {
        for (user_data, nonces) in [
            (Some(vec![1, 2, 3]), None),
            (None, Some(vec![])),
            (None, Some(vec![vec![1; 31]])),
            (None, Some(vec![vec![1; 32], vec![2; 32]])),
        ] {
            let err = EnclaveApiServer::signer_attestation(&test_rpc(), user_data, nonces)
                .await
                .unwrap_err();

            assert_eq!(err.code(), -32602);
        }
    }

    #[test]
    fn rpc_module_exposes_registrar_methods_only() {
        let module = test_rpc().into_rpc_module().unwrap();
        let methods: Vec<_> = module.method_names().collect();

        assert!(methods.contains(&"healthz"));
        assert!(methods.contains(&"enclave_signerPublicKey"));
        assert!(methods.contains(&"enclave_signerAttestation"));
        assert!(!methods.contains(&"prover_prove"));
    }
}
