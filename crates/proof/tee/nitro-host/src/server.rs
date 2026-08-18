use std::{fmt, net::SocketAddr, sync::Arc};

use alloy_signer::utils::public_key_to_address;
use base_health::{HealthzApiServer, HealthzRpc};
use base_proof_host::ProverConfig;
use base_proof_primitives::{
    AttestedWithdrawalApiServer, EnclaveApiServer, ProofRequest, ProofResult, ProverApiServer,
};
use jsonrpsee::{
    RpcModule,
    core::{RpcResult, async_trait},
    server::{Server, ServerHandle, middleware::http::ProxyGetRequestLayer},
};
use k256::ecdsa::VerifyingKey;
use tracing::{info, warn};

use super::{
    health::{RegistrationHealthConfig, RegistrationHealthzRpc},
    pool::{NitroEnclavePool, NitroEnclavePoolError},
    registration::RegistrationChecker,
    transport::NitroTransport,
};

/// Maximum allowed size for the `user_data` attestation field (NSM limit).
const MAX_USER_DATA_BYTES: usize = 512;

/// Maximum allowed size for the `nonce` attestation field (NSM limit).
const MAX_NONCE_BYTES: usize = 512;

/// Maximum number of trie nodes accepted for one attested-withdrawal proof.
const MAX_STORAGE_PROOF_NODES: usize = 64;

/// Maximum encoded trie-node bytes accepted for one attested-withdrawal proof.
const MAX_STORAGE_PROOF_BYTES: usize = 1024 * 1024;

/// Host-side TEE prover server exposing a JSON-RPC interface.
///
/// Implements two JSON-RPC namespaces:
/// - `prover_*`: proving operations (forwarded to the enclave via transport)
/// - `enclave_*`: signer info queries (also forwarded via transport)
pub struct NitroProverServer {
    pool: NitroEnclavePool,
    registration_health: Option<RegistrationHealthConfig>,
}

impl fmt::Debug for NitroProverServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NitroProverServer").finish_non_exhaustive()
    }
}

impl NitroProverServer {
    fn rpc_err(code: i32, err: impl std::fmt::Display) -> jsonrpsee::types::ErrorObjectOwned {
        jsonrpsee::types::ErrorObjectOwned::owned(code, err.to_string(), None::<()>)
    }

    fn pool_err(err: NitroEnclavePoolError) -> jsonrpsee::types::ErrorObjectOwned {
        match err {
            NitroEnclavePoolError::Registration(e) => {
                warn!(error = %e, "rejecting proof request: signer validation failed");
                Self::rpc_err(-32001, e)
            }
            NitroEnclavePoolError::Busy => Self::rpc_err(-32002, err),
            NitroEnclavePoolError::RegistrationCheckerMismatch { .. }
            | NitroEnclavePoolError::Prover(_) => Self::rpc_err(-32000, err),
        }
    }

    /// Create a server with the given prover config and enclave transport.
    pub fn new(config: ProverConfig, transport: Arc<NitroTransport>) -> Self {
        Self::new_multi(config, vec![transport])
    }

    /// Create a server with multiple enclave transports for dual-enclave deployments.
    ///
    /// # Panics
    ///
    /// Panics if `transports` is empty.
    pub fn new_multi(config: ProverConfig, transports: Vec<Arc<NitroTransport>>) -> Self {
        let pool = NitroEnclavePool::new_multi(config, transports);
        Self { pool, registration_health: None }
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
        let transports = self.pool.transports();
        let mut pool = self.pool;

        let checker = match self.registration_health {
            Some(config) => {
                info!(
                    registry = %config.registry_address,
                    "registration-gated health and proving guard enabled"
                );
                let checker = Arc::new(
                    RegistrationChecker::from_health_config(transports.clone(), &config)
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

        let attested_withdrawal_checker = checker.as_ref().map(Arc::clone);
        if let Some(checker) = checker {
            pool = pool
                .with_registration_checker(checker)
                .map_err(|e| eyre::eyre!("registration checker init failed: {e}"))?;
        }
        module.merge(NitroProverRpc { pool: Arc::new(pool) }.into_rpc())?;

        module.merge(NitroSignerRpc { transports: transports.clone() }.into_rpc())?;
        module.merge(
            NitroAttestedWithdrawalRpc {
                transports,
                registration_checker: attested_withdrawal_checker,
            }
            .into_rpc(),
        )?;

        Ok(server.start(module))
    }

    /// Start the registrar-facing signer API without exposing proof execution.
    pub async fn run_registrar_rpc_server(
        addr: SocketAddr,
        transports: Vec<Arc<NitroTransport>>,
        registration_checker: Option<Arc<RegistrationChecker>>,
    ) -> eyre::Result<ServerHandle> {
        let middleware = tower::ServiceBuilder::new()
            .layer(ProxyGetRequestLayer::new([("/healthz", "healthz")])?);
        let server = Server::builder().set_http_middleware(middleware).build(addr).await?;
        let addr = server.local_addr()?;
        info!(addr = %addr, "nitro registrar rpc server started");

        let mut module = RpcModule::new(());
        match registration_checker.as_ref() {
            Some(checker) => {
                module.merge(
                    RegistrationHealthzRpc::new(env!("CARGO_PKG_VERSION"), Arc::clone(checker))
                        .into_rpc(),
                )?;
            }
            None => {
                module.merge(HealthzRpc::new(env!("CARGO_PKG_VERSION")).into_rpc())?;
            }
        }
        module.merge(NitroSignerRpc { transports }.into_rpc())?;

        Ok(server.start(module))
    }

    /// Start the private signer API used for attested-withdrawal authorizations.
    pub async fn run_attested_withdrawal_rpc_server(
        addr: SocketAddr,
        transports: Vec<Arc<NitroTransport>>,
        registration_checker: Option<Arc<RegistrationChecker>>,
    ) -> eyre::Result<ServerHandle> {
        let middleware = tower::ServiceBuilder::new()
            .layer(ProxyGetRequestLayer::new([("/healthz", "healthz")])?);
        let server = Server::builder().set_http_middleware(middleware).build(addr).await?;
        let addr = server.local_addr()?;
        info!(addr = %addr, "nitro attested-withdrawal rpc server started");

        let mut module = RpcModule::new(());
        match registration_checker.as_ref() {
            Some(checker) => {
                module.merge(
                    RegistrationHealthzRpc::new(env!("CARGO_PKG_VERSION"), Arc::clone(checker))
                        .into_rpc(),
                )?;
            }
            None => {
                module.merge(HealthzRpc::new(env!("CARGO_PKG_VERSION")).into_rpc())?;
            }
        }
        module.merge(NitroSignerRpc { transports: transports.clone() }.into_rpc())?;
        module.merge(NitroAttestedWithdrawalRpc { transports, registration_checker }.into_rpc())?;

        Ok(server.start(module))
    }
}

/// Inner RPC handler for `prover_*` methods.
struct NitroProverRpc {
    pool: Arc<NitroEnclavePool>,
}

#[async_trait]
impl ProverApiServer for NitroProverRpc {
    async fn prove(&self, request: ProofRequest) -> RpcResult<ProofResult> {
        self.pool.prove(request).await.map_err(NitroProverServer::pool_err)
    }
}

/// Inner RPC handler for `enclave_*` methods.
///
/// All-or-nothing: both `signer_public_key` and `signer_attestation` fail if
/// **any** transport is unreachable.  Callers need the complete set of keys /
/// attestations (one per enclave) to register all signers onchain, so a
/// partial response would be unusable.
struct NitroSignerRpc {
    transports: Vec<Arc<NitroTransport>>,
}

#[async_trait]
impl EnclaveApiServer for NitroSignerRpc {
    async fn signer_public_key(&self) -> RpcResult<Vec<Vec<u8>>> {
        let mut keys = Vec::with_capacity(self.transports.len());
        for transport in &self.transports {
            keys.push(
                transport
                    .signer_public_key()
                    .await
                    .map_err(|e| NitroProverServer::rpc_err(-32001, e))?,
            );
        }
        // Per-call signer log so an investigator can trace every signer
        // the host has ever returned to the registrar. Makes a silent
        // mid-run enclave re-key visible as a sequence of log lines
        // with changing addresses.
        let signers: Vec<String> = keys
            .iter()
            .map(|k| {
                VerifyingKey::from_sec1_bytes(k)
                    .map(|vk| format!("{}", public_key_to_address(&vk)))
                    .unwrap_or_else(|e| {
                        warn!(error = %e, "failed to parse enclave signer public key");
                        "<unparseable>".to_string()
                    })
            })
            .collect();
        info!(signers = ?signers, "nitro_host.signer_public_key_rpc");
        Ok(keys)
    }

    async fn signer_attestation(
        &self,
        user_data: Option<Vec<u8>>,
        nonces: Option<Vec<Vec<u8>>>,
    ) -> RpcResult<Vec<Vec<u8>>> {
        // NSM limits: user_data ≤ 512 bytes, each nonce ≤ 512 bytes.
        // Reject oversized payloads early to avoid allocating and forwarding them
        // through the vsock transport only to be rejected by the enclave.
        if user_data.as_ref().is_some_and(|d| d.len() > MAX_USER_DATA_BYTES) {
            return Err(NitroProverServer::rpc_err(
                -32602,
                format!("user_data exceeds {MAX_USER_DATA_BYTES}-byte limit"),
            ));
        }
        if nonces.as_ref().is_some_and(|items| items.len() != self.transports.len()) {
            return Err(NitroProverServer::rpc_err(
                -32602,
                format!("nonces length must equal signer count {}", self.transports.len()),
            ));
        }
        if nonces.as_ref().is_some_and(|items| items.iter().any(|n| n.len() > MAX_NONCE_BYTES)) {
            return Err(NitroProverServer::rpc_err(
                -32602,
                format!("nonce exceeds {MAX_NONCE_BYTES}-byte limit"),
            ));
        }

        let mut attestations = Vec::with_capacity(self.transports.len());
        for (index, transport) in self.transports.iter().enumerate() {
            let nonce = nonces.as_ref().map(|items| items[index].clone());
            attestations.push(
                transport
                    .signer_attestation(user_data.clone(), nonce)
                    .await
                    .map_err(|e| NitroProverServer::rpc_err(-32001, e))?,
            );
        }
        Ok(attestations)
    }
}

/// Inner RPC handler for attested-withdrawal signing on the private prover endpoint.
///
/// A withdrawal needs one signature, so this handler uses the primary (first)
/// transport even when the deployment has multiple enclaves.
struct NitroAttestedWithdrawalRpc {
    transports: Vec<Arc<NitroTransport>>,
    registration_checker: Option<Arc<RegistrationChecker>>,
}

impl NitroAttestedWithdrawalRpc {
    async fn select_transport(
        &self,
    ) -> Result<&Arc<NitroTransport>, jsonrpsee::types::ErrorObjectOwned> {
        let index = if let Some(checker) = &self.registration_checker {
            checker
                .select_all_valid_enclaves()
                .await
                .map_err(|error| NitroProverServer::rpc_err(-32001, error))?
                .first()
                .expect("select_all_valid_enclaves returns at least one signer")
                .index
        } else {
            0
        };
        self.transports.get(index).ok_or_else(|| {
            NitroProverServer::rpc_err(-32001, "selected enclave transport is unavailable")
        })
    }
}

#[async_trait]
impl AttestedWithdrawalApiServer for NitroAttestedWithdrawalRpc {
    async fn sign_attested_withdrawal(
        &self,
        auth_hash: alloy_primitives::B256,
        message_passer_storage_root: alloy_primitives::B256,
        storage_proof: Vec<alloy_primitives::Bytes>,
    ) -> RpcResult<Vec<u8>> {
        if storage_proof.len() > MAX_STORAGE_PROOF_NODES {
            return Err(NitroProverServer::rpc_err(
                -32602,
                format!("storage proof exceeds {MAX_STORAGE_PROOF_NODES}-node limit"),
            ));
        }
        let storage_proof_bytes = storage_proof
            .iter()
            .try_fold(0_usize, |total, node| total.checked_add(node.len()).ok_or(()));
        match storage_proof_bytes {
            Ok(bytes) if bytes <= MAX_STORAGE_PROOF_BYTES => {}
            Ok(_) | Err(()) => {
                return Err(NitroProverServer::rpc_err(
                    -32602,
                    format!("storage proof exceeds {MAX_STORAGE_PROOF_BYTES}-byte limit"),
                ));
            }
        }

        let transport = self.select_transport().await?;
        transport
            .sign_attested_withdrawal(auth_hash, message_passer_storage_root, storage_proof)
            .await
            .map_err(|error| NitroProverServer::rpc_err(-32001, error))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::atomic::Ordering};

    use alloy_signer::utils::public_key_to_address;
    use base_proof_primitives::{AttestedWithdrawalApiServer, EnclaveApiServer};
    use base_proof_tee_nitro_enclave::Server as EnclaveServer;
    use jsonrpsee::core::client::ClientT as _;
    use k256::ecdsa::VerifyingKey;

    use super::*;
    use crate::test_utils::{AddressBasedMockRegistry, MockRegistry};

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
    async fn registrar_rpc_server_exposes_signer_api() {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(Arc::clone(&server)));
        let expected = server.signer_public_key();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let handle =
            NitroProverServer::run_registrar_rpc_server(addr, vec![transport], None).await.unwrap();
        let client = jsonrpsee::http_client::HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .unwrap();

        let result: Vec<Vec<u8>> =
            client.request("enclave_signerPublicKey", jsonrpsee::rpc_params![]).await.unwrap();
        assert_eq!(result, vec![expected]);
        handle.stop().unwrap();
    }

    #[tokio::test]
    async fn registrar_rpc_server_does_not_expose_attested_withdrawal_signing() {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let handle =
            NitroProverServer::run_registrar_rpc_server(addr, vec![transport], None).await.unwrap();
        let client = jsonrpsee::http_client::HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .unwrap();

        let error = client
            .request::<Vec<u8>, _>(
                "enclave_signAttestedWithdrawal",
                jsonrpsee::rpc_params![
                    alloy_primitives::B256::ZERO,
                    alloy_primitives::B256::ZERO,
                    Vec::<alloy_primitives::Bytes>::new()
                ],
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Method not found"));
        handle.stop().unwrap();
    }

    #[tokio::test]
    async fn attested_withdrawal_rpc_rejects_oversized_storage_proof() {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let rpc = NitroAttestedWithdrawalRpc {
            transports: vec![Arc::new(NitroTransport::local(server))],
            registration_checker: None,
        };
        let storage_proof = vec![alloy_primitives::Bytes::new(); MAX_STORAGE_PROOF_NODES + 1];

        let error = AttestedWithdrawalApiServer::sign_attested_withdrawal(
            &rpc,
            alloy_primitives::B256::ZERO,
            alloy_primitives::B256::ZERO,
            storage_proof,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), -32602);
        assert!(error.message().contains("storage proof"));
    }

    #[tokio::test]
    async fn attested_withdrawal_rpc_selects_a_registered_signer() {
        let first_server = Arc::new(EnclaveServer::new_local().unwrap());
        let second_server = Arc::new(EnclaveServer::new_local().unwrap());
        let first = Arc::new(NitroTransport::local(first_server));
        let second = Arc::new(NitroTransport::local(second_server));
        let second_key = second.signer_public_key().await.unwrap();
        let second_address =
            public_key_to_address(&VerifyingKey::from_sec1_bytes(&second_key).unwrap());
        let checker = Arc::new(
            RegistrationChecker::new(
                vec![Arc::clone(&first), Arc::clone(&second)],
                AddressBasedMockRegistry::new(HashMap::from([(second_address, true)])),
            )
            .unwrap(),
        );
        let rpc = NitroAttestedWithdrawalRpc {
            transports: vec![first, Arc::clone(&second)],
            registration_checker: Some(checker),
        };

        assert!(Arc::ptr_eq(rpc.select_transport().await.unwrap(), &second));
    }

    #[tokio::test]
    async fn registrar_rpc_server_uses_shared_registration_checker() {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let registry = MockRegistry::new(true);
        let call_count = Arc::clone(&registry.call_count);
        let checker = Arc::new(
            RegistrationChecker::new(vec![Arc::clone(&transport)], registry.clone()).unwrap(),
        );
        assert!(checker.check_health().await.unwrap());
        registry.should_fail.store(true, Ordering::Relaxed);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let handle = NitroProverServer::run_registrar_rpc_server(
            addr,
            vec![transport],
            Some(Arc::clone(&checker)),
        )
        .await
        .unwrap();
        let client = jsonrpsee::http_client::HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .unwrap();

        let result: base_health::HealthzResponse =
            client.request("healthz", jsonrpsee::rpc_params![]).await.unwrap();
        assert_eq!(result.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(call_count.load(Ordering::Relaxed), 1);
        handle.stop().unwrap();
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
        let result = EnclaveApiServer::signer_attestation(&rpc, None, Some(vec![oversized])).await;
        let err = result.unwrap_err();
        assert_eq!(err.code(), -32602);
        assert!(err.message().contains("nonce"));
    }

    #[tokio::test]
    async fn signer_attestation_rejects_nonce_count_mismatch() {
        let server_a = Arc::new(EnclaveServer::new_local().unwrap());
        let server_b = Arc::new(EnclaveServer::new_local().unwrap());
        let rpc = NitroSignerRpc {
            transports: vec![
                Arc::new(NitroTransport::local(server_a)),
                Arc::new(NitroTransport::local(server_b)),
            ],
        };

        let result =
            EnclaveApiServer::signer_attestation(&rpc, None, Some(vec![vec![0u8; 32]])).await;
        let err = result.unwrap_err();
        assert_eq!(err.code(), -32602);
        assert!(err.message().contains("nonces length"));
    }

    #[test]
    fn pool_busy_error_maps_to_retryable_rpc_code() {
        let err = NitroProverServer::pool_err(NitroEnclavePoolError::Busy);
        assert_eq!(err.code(), -32002);
        assert!(err.message().contains("enclave busy"));
    }
}
