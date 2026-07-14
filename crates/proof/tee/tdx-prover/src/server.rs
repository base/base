use std::{net::SocketAddr, sync::Arc};

use alloy_primitives::B256;
use base_health::{HealthzApiServer, HealthzRpc};
use base_proof_primitives::EnclaveApiServer;
use base_proof_tee_tdx_runtime::{TdxAttestationContext, TdxRuntime};
use base_proof_tee_tdx_verifier::TdxVerifier;
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
    context: Option<TdxAttestationContext>,
}

impl TdxProverServer {
    /// Create a registrar-facing server for one TDX runtime.
    pub const fn new(runtime: Arc<TdxRuntime>, context: Option<TdxAttestationContext>) -> Self {
        Self { runtime, context }
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

        let Some(nonces) = nonces else {
            return Err(jsonrpsee::types::ErrorObjectOwned::owned(
                -32602,
                "TDX signer attestations require exactly one nonce",
                None::<()>,
            ));
        };
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
        let attestation_nonce = Some(B256::from(nonce));

        let context = self.context.ok_or_else(|| {
            jsonrpsee::types::ErrorObjectOwned::owned(
                -32001,
                "TDX signer registration context is not configured",
                None::<()>,
            )
        })?;
        let signer_public_key = self.runtime.signer_public_key();
        let token_nonce = attestation_nonce
            .map(|nonce| {
                TdxVerifier::token_nonce(
                    &signer_public_key,
                    nonce,
                    context.chain_id,
                    context.registry_address,
                )
            })
            .transpose()
            .map_err(|error| {
                jsonrpsee::types::ErrorObjectOwned::owned(-32001, error.to_string(), None::<()>)
            })?;
        let token = self.runtime.attestation_token(token_nonce).map_err(|error| {
            jsonrpsee::types::ErrorObjectOwned::owned(-32001, error.to_string(), None::<()>)
        })?;
        Ok(vec![
            TdxSignerAttestation {
                signer_public_key,
                token,
                attestation_nonce,
                chain_id: context.chain_id,
                registry_address: context.registry_address,
            }
            .encode(),
        ])
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use alloy_primitives::Bytes;
    use base_proof_primitives::EnclaveApiServer;
    use base_proof_tee_tdx_runtime::{Result as RuntimeResult, TdxAttestationTokenProvider};

    use super::*;

    #[derive(Debug, Default)]
    struct TestTokenProvider {
        nonces: Arc<Mutex<Vec<String>>>,
    }

    impl TdxAttestationTokenProvider for TestTokenProvider {
        fn token(&self, _audience: &str, nonces: &[String]) -> RuntimeResult<Bytes> {
            *self.nonces.lock().unwrap() = nonces.to_vec();
            Ok(Bytes::from_static(b"fixture-token"))
        }
    }

    fn test_rpc() -> (TdxProverServer, Arc<Mutex<Vec<String>>>) {
        let token_provider = TestTokenProvider::default();
        let nonces = Arc::clone(&token_provider.nonces);
        let server = TdxProverServer::new(
            Arc::new(TdxRuntime::new(token_provider, "base-tdx-prover")),
            Some(TdxAttestationContext {
                chain_id: 11_155_111,
                registry_address: alloy_primitives::Address::repeat_byte(0x33),
            }),
        );
        (server, nonces)
    }

    #[tokio::test]
    async fn signer_attestation_binds_registrar_nonce() {
        let (rpc, token_nonces) = test_rpc();
        let nonce = vec![0x11; 32];
        let result = EnclaveApiServer::signer_attestation(&rpc, None, Some(vec![nonce.clone()]))
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let attestation = TdxSignerAttestation::decode(&result[0]).unwrap();
        assert_eq!(attestation.signer_public_key, rpc.runtime.signer_public_key().to_vec());
        assert_eq!(attestation.attestation_nonce, Some(B256::from([0x11; 32])));
        assert_eq!(attestation.token, Bytes::from_static(b"fixture-token"));
        assert_eq!(attestation.chain_id, 11_155_111);
        assert_eq!(attestation.registry_address, alloy_primitives::Address::repeat_byte(0x33));
        assert_eq!(
            *token_nonces.lock().unwrap(),
            vec![alloy_primitives::hex::encode(
                TdxVerifier::token_nonce(
                    &attestation.signer_public_key,
                    B256::from([0x11; 32]),
                    attestation.chain_id,
                    attestation.registry_address,
                )
                .unwrap()
            )]
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
            let (rpc, _) = test_rpc();
            let err =
                EnclaveApiServer::signer_attestation(&rpc, user_data, nonces).await.unwrap_err();

            assert_eq!(err.code(), -32602);
        }
    }

    #[tokio::test]
    async fn signer_attestation_requires_registration_context() {
        let token_provider = TestTokenProvider::default();
        let rpc = TdxProverServer::new(
            Arc::new(TdxRuntime::new(token_provider, "base-tdx-prover")),
            None,
        );

        let error = EnclaveApiServer::signer_attestation(&rpc, None, Some(vec![vec![0x11; 32]]))
            .await
            .unwrap_err();

        assert_eq!(error.code(), -32001);
    }

    #[test]
    fn rpc_module_exposes_registrar_methods_only() {
        let (rpc, _) = test_rpc();
        let module = rpc.into_rpc_module().unwrap();
        let methods: Vec<_> = module.method_names().collect();

        assert!(methods.contains(&"healthz"));
        assert!(methods.contains(&"enclave_signerPublicKey"));
        assert!(methods.contains(&"enclave_signerAttestation"));
        assert!(!methods.contains(&"prover_prove"));
    }
}
