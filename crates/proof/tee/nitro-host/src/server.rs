use std::{fmt, net::SocketAddr, sync::Arc, time::Duration};

use alloy_signer::utils::public_key_to_address;
use base_health::{HealthzApiServer, HealthzRpc};
use base_proof_contracts::TEEProverRegistryContractClient;
use base_proof_host::{ProverConfig, ProverService};
use base_proof_primitives::{EnclaveApiServer, ProofRequest, ProofResult, ProverApiServer};
use jsonrpsee::{
    RpcModule,
    core::{RpcResult, async_trait},
    server::{Server, ServerHandle, middleware::http::ProxyGetRequestLayer},
};
use k256::ecdsa::VerifyingKey;
use tokio::sync::Semaphore;
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

/// Maximum number of concurrent `prover_prove` requests per enclave.
///
/// Proving is CPU- and memory-intensive. The enclave does not reject concurrent requests itself;
/// it would serialize them under load, adding queueing latency and resource pressure. We enforce
/// the limit host-side and reject excess requests with `-32002` so callers can back off and retry
/// instead of piling up.
const MAX_CONCURRENT_PROOF_REQUESTS_PER_ENCLAVE: usize = 1;

struct EnclaveService {
    transport: Arc<NitroTransport>,
    service: ProverService<NitroBackend>,
    prove_permit: Arc<Semaphore>,
}

/// Host-side TEE prover server exposing a JSON-RPC interface.
///
/// Implements two JSON-RPC namespaces:
/// - `prover_*`: proving operations (forwarded to the enclave via transport)
/// - `enclave_*`: signer info queries (also forwarded via transport)
pub struct NitroProverServer {
    enclaves: Vec<EnclaveService>,
    proof_request_timeout: Duration,
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

    /// Create a server with the given prover config, enclave transport, and proof request timeout.
    pub fn new(
        config: ProverConfig,
        transport: Arc<NitroTransport>,
        proof_request_timeout: Duration,
    ) -> Self {
        Self::new_multi(config, vec![transport], proof_request_timeout)
    }

    /// Create a server with multiple enclave transports for dual-enclave deployments.
    ///
    /// # Panics
    ///
    /// Panics if `transports` is empty.
    pub fn new_multi(
        config: ProverConfig,
        transports: Vec<Arc<NitroTransport>>,
        proof_request_timeout: Duration,
    ) -> Self {
        assert!(!transports.is_empty(), "at least one transport is required");
        let enclaves = transports
            .into_iter()
            .map(|transport| {
                let backend = NitroBackend::new(Arc::clone(&transport));
                EnclaveService {
                    transport,
                    service: ProverService::new(config.clone(), backend),
                    prove_permit: Arc::new(Semaphore::new(
                        MAX_CONCURRENT_PROOF_REQUESTS_PER_ENCLAVE,
                    )),
                }
            })
            .collect();
        Self { enclaves, proof_request_timeout, registration_health: None }
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
        let transports: Vec<Arc<NitroTransport>> =
            self.enclaves.iter().map(|enclave| Arc::clone(&enclave.transport)).collect();

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
                    RegistrationChecker::new(transports.clone(), registry)
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
                enclaves: self.enclaves,
                proof_request_timeout: self.proof_request_timeout,
                checker,
            }
            .into_rpc(),
        )?;

        module.merge(NitroSignerRpc { transports }.into_rpc())?;

        Ok(server.start(module))
    }
}

/// Inner RPC handler for `prover_*` methods.
struct NitroProverRpc {
    enclaves: Vec<EnclaveService>,
    proof_request_timeout: Duration,
    checker: Option<Arc<RegistrationChecker>>,
}

impl NitroProverRpc {
    /// Pick the first valid enclave with an available permit. Falling through busy enclaves
    /// matters in dual-enclave deployments: without fall-through a single in-flight request
    /// would make idle enclaves unreachable even though they are valid and available.
    async fn acquire_enclave(
        &self,
        l2_block: u64,
    ) -> RpcResult<(&EnclaveService, tokio::sync::OwnedSemaphorePermit)> {
        let candidate_indices: Vec<usize> = match &self.checker {
            Some(checker) => checker
                .select_all_valid_enclaves()
                .await
                .map_err(|e| {
                    warn!(error = %e, "rejecting proof request: signer validation failed");
                    NitroProverServer::rpc_err(-32001, e)
                })?
                .into_iter()
                .map(|v| v.index)
                .collect(),
            // Constructor guarantees at least one enclave.
            None => vec![0],
        };

        candidate_indices
            .iter()
            .find_map(|&i| {
                Arc::clone(&self.enclaves[i].prove_permit)
                    .try_acquire_owned()
                    .ok()
                    .map(|permit| (&self.enclaves[i], permit))
            })
            .ok_or_else(|| {
                warn!(l2_block, "rejecting proof request: all valid enclaves already proving");
                NitroProverServer::rpc_err(
                    -32002,
                    "enclave busy: another proof request is already in flight",
                )
            })
    }
}

#[async_trait]
impl ProverApiServer for NitroProverRpc {
    async fn prove(&self, request: ProofRequest) -> RpcResult<ProofResult> {
        let l2_block = request.claimed_l2_block_number;
        let timeout = self.proof_request_timeout;

        let (enclave, _permit) = self.acquire_enclave(l2_block).await?;

        match tokio::time::timeout(timeout, enclave.service.prove_block(request)).await {
            Ok(result) => result.map_err(|e| NitroProverServer::rpc_err(-32000, e)),
            Err(_elapsed) => {
                warn!(l2_block, timeout_secs = timeout.as_secs(), "proof request timed out");
                Err(NitroProverServer::rpc_err(
                    -32000,
                    format!(
                        "proof request timed out after {}s for L2 block {l2_block}",
                        timeout.as_secs()
                    ),
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
        nonce: Option<Vec<u8>>,
    ) -> RpcResult<Vec<Vec<u8>>> {
        // NSM limits: user_data ≤ 512 bytes, nonce ≤ 512 bytes.
        // Reject oversized payloads early to avoid allocating and forwarding them
        // through the vsock transport only to be rejected by the enclave.
        if user_data.as_ref().is_some_and(|d| d.len() > MAX_USER_DATA_BYTES) {
            return Err(NitroProverServer::rpc_err(
                -32602,
                format!("user_data exceeds {MAX_USER_DATA_BYTES}-byte limit"),
            ));
        }
        if nonce.as_ref().is_some_and(|n| n.len() > MAX_NONCE_BYTES) {
            return Err(NitroProverServer::rpc_err(
                -32602,
                format!("nonce exceeds {MAX_NONCE_BYTES}-byte limit"),
            ));
        }

        let mut attestations = Vec::with_capacity(self.transports.len());
        for transport in &self.transports {
            attestations.push(
                transport
                    .signer_attestation(user_data.clone(), nonce.clone())
                    .await
                    .map_err(|e| NitroProverServer::rpc_err(-32001, e))?,
            );
        }
        Ok(attestations)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_genesis::ChainConfig;
    use alloy_signer::utils::public_key_to_address;
    use base_common_genesis::RollupConfig;
    use base_proof_primitives::EnclaveApiServer;
    use base_proof_tee_nitro_enclave::Server as EnclaveServer;
    use k256::ecdsa::VerifyingKey;

    use super::*;
    use crate::test_utils::AddressBasedMockRegistry;

    fn test_prover_config() -> ProverConfig {
        ProverConfig {
            l1_eth_url: "http://127.0.0.1:1".to_string(),
            l2_eth_url: "http://127.0.0.1:1".to_string(),
            l1_beacon_url: "http://127.0.0.1:1".to_string(),
            l2_chain_id: 0,
            rollup_config: RollupConfig::default(),
            l1_config: ChainConfig::default(),
            enable_experimental_witness_endpoint: false,
        }
    }

    fn test_prover_rpc() -> NitroProverRpc {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let backend = NitroBackend::new(Arc::clone(&transport));
        let service = ProverService::new(test_prover_config(), backend);
        let enclave = EnclaveService {
            transport,
            service,
            prove_permit: Arc::new(Semaphore::new(MAX_CONCURRENT_PROOF_REQUESTS_PER_ENCLAVE)),
        };
        NitroProverRpc {
            enclaves: vec![enclave],
            proof_request_timeout: Duration::from_secs(60),
            checker: None,
        }
    }

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

    #[tokio::test]
    async fn prove_rejects_concurrent_request_when_permit_held() {
        let rpc = test_prover_rpc();

        let permit = Arc::clone(&rpc.enclaves[0].prove_permit)
            .try_acquire_owned()
            .expect("permit should be available before first acquire");

        let err = rpc.prove(ProofRequest::default()).await.unwrap_err();
        assert_eq!(err.code(), -32002);
        assert!(err.message().contains("enclave busy"));

        drop(permit);
        assert_eq!(rpc.enclaves[0].prove_permit.available_permits(), 1);
    }

    #[tokio::test]
    async fn prove_permit_is_released_when_handle_dropped() {
        let rpc = test_prover_rpc();

        let (_enclave, permit) = rpc.acquire_enclave(0).await.unwrap();
        assert_eq!(rpc.enclaves[0].prove_permit.available_permits(), 0);

        drop(permit);
        assert_eq!(
            rpc.enclaves[0].prove_permit.available_permits(),
            MAX_CONCURRENT_PROOF_REQUESTS_PER_ENCLAVE,
            "permit must be released when the RAII handle is dropped"
        );
    }

    #[tokio::test]
    async fn prove_permit_is_per_enclave_in_multi_enclave_setup() {
        let mut rpc = test_prover_rpc();
        let second = Arc::new(EnclaveServer::new_local().unwrap());
        let second_transport = Arc::new(NitroTransport::local(second));
        let second_backend = NitroBackend::new(Arc::clone(&second_transport));
        let second_service = ProverService::new(test_prover_config(), second_backend);
        rpc.enclaves.push(EnclaveService {
            transport: second_transport,
            service: second_service,
            prove_permit: Arc::new(Semaphore::new(MAX_CONCURRENT_PROOF_REQUESTS_PER_ENCLAVE)),
        });

        let _held = Arc::clone(&rpc.enclaves[0].prove_permit).try_acquire_owned().unwrap();

        assert_eq!(rpc.enclaves[1].prove_permit.available_permits(), 1);
    }

    #[tokio::test]
    async fn prove_falls_through_to_second_enclave_when_first_is_busy() {
        async fn signer_for(transport: &NitroTransport) -> alloy_primitives::Address {
            let pk = transport.signer_public_key().await.unwrap();
            let vk = VerifyingKey::from_sec1_bytes(&pk).unwrap();
            public_key_to_address(&vk)
        }

        let s1 = Arc::new(EnclaveServer::new_local().unwrap());
        let s2 = Arc::new(EnclaveServer::new_local().unwrap());
        let t1 = Arc::new(NitroTransport::local(s1));
        let t2 = Arc::new(NitroTransport::local(s2));

        let addr1 = signer_for(&t1).await;
        let addr2 = signer_for(&t2).await;

        let mut map = HashMap::new();
        map.insert(addr1, true);
        map.insert(addr2, true);
        let registry = AddressBasedMockRegistry::new(map);

        let checker = Arc::new(
            RegistrationChecker::new(vec![Arc::clone(&t1), Arc::clone(&t2)], registry).unwrap(),
        );

        let enclave0 = EnclaveService {
            transport: Arc::clone(&t1),
            service: ProverService::new(test_prover_config(), NitroBackend::new(Arc::clone(&t1))),
            prove_permit: Arc::new(Semaphore::new(MAX_CONCURRENT_PROOF_REQUESTS_PER_ENCLAVE)),
        };
        let enclave1 = EnclaveService {
            transport: Arc::clone(&t2),
            service: ProverService::new(test_prover_config(), NitroBackend::new(Arc::clone(&t2))),
            prove_permit: Arc::new(Semaphore::new(MAX_CONCURRENT_PROOF_REQUESTS_PER_ENCLAVE)),
        };
        let rpc = NitroProverRpc {
            enclaves: vec![enclave0, enclave1],
            proof_request_timeout: Duration::from_secs(60),
            checker: Some(checker),
        };

        // Saturate the first enclave; the second is still free.
        let _held = Arc::clone(&rpc.enclaves[0].prove_permit).try_acquire_owned().unwrap();

        // acquire_enclave must select enclave[1] since enclave[0] has no permits. We test the
        // selection helper directly because prove_block requires a live beacon endpoint.
        let (enclave, _permit) = rpc.acquire_enclave(0).await.expect("fall-through to enclave[1]");
        assert!(
            Arc::ptr_eq(&enclave.transport, &rpc.enclaves[1].transport),
            "expected enclave[1] selected via fall-through"
        );
        assert_eq!(rpc.enclaves[1].prove_permit.available_permits(), 0);
    }
}
