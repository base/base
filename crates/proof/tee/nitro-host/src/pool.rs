//! Reusable Nitro enclave proof pool.

use std::{fmt, sync::Arc};

use base_proof_host::{ProverConfig, ProverError, ProverService};
use base_proof_primitives::{ProofRequest, ProofResult};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::warn;

use super::{
    NitroBackend,
    registration::{RegistrationChecker, RegistrationError},
    transport::NitroTransport,
};

/// Maximum number of concurrent proof requests per enclave.
///
/// Proving is CPU- and memory-intensive. The enclave does not reject concurrent
/// requests itself; it would serialize them under load, adding queueing latency
/// and resource pressure. The pool enforces the limit host-side so callers can
/// back off and retry instead of piling up.
pub const MAX_CONCURRENT_PROOF_REQUESTS_PER_ENCLAVE: usize = 1;

struct EnclaveService {
    transport: Arc<NitroTransport>,
    service: ProverService<NitroBackend>,
    prove_permit: Arc<Semaphore>,
}

/// Reusable host-side pool for proving through one or more Nitro enclaves.
///
/// The pool owns enclave transports, per-enclave proving services, per-enclave
/// concurrency permits, optional registration checks, and multi-enclave signer
/// selection.
pub struct NitroEnclavePool {
    enclaves: Vec<EnclaveService>,
    checker: Option<Arc<RegistrationChecker>>,
}

impl fmt::Debug for NitroEnclavePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NitroEnclavePool")
            .field("enclave_count", &self.enclaves.len())
            .field("has_registration_checker", &self.checker.is_some())
            .finish()
    }
}

impl NitroEnclavePool {
    /// Create a pool with one enclave transport.
    pub fn new(config: ProverConfig, transport: Arc<NitroTransport>) -> Self {
        Self::new_multi(config, vec![transport])
    }

    /// Create a pool with multiple enclave transports.
    ///
    /// # Panics
    ///
    /// Panics if `transports` is empty.
    pub fn new_multi(config: ProverConfig, transports: Vec<Arc<NitroTransport>>) -> Self {
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

        Self { enclaves, checker: None }
    }

    /// Attach a registration checker used to select valid enclave signers.
    ///
    /// The checker must have been built from the exact same transport `Arc`s
    /// in the same order as this pool. The valid-signer indices returned by
    /// the checker are relative to that list, so accepting a different list
    /// could route proofs to an enclave that was not validated.
    pub fn with_registration_checker(
        mut self,
        checker: Arc<RegistrationChecker>,
    ) -> Result<Self, NitroEnclavePoolError> {
        self.validate_registration_checker(&checker)?;
        self.checker = Some(checker);
        Ok(self)
    }

    /// Returns the configured enclave transports.
    pub fn transports(&self) -> Vec<Arc<NitroTransport>> {
        self.enclaves.iter().map(|enclave| Arc::clone(&enclave.transport)).collect()
    }

    /// Returns the registration checker if one is configured.
    pub fn registration_checker(&self) -> Option<Arc<RegistrationChecker>> {
        self.checker.as_ref().map(Arc::clone)
    }

    /// Proves one request using the busy-enclave policy.
    pub async fn prove(&self, request: ProofRequest) -> Result<ProofResult, NitroEnclavePoolError> {
        let l2_block = request.claimed_l2_block_number;
        let (enclave, _permit) = self.acquire_enclave(l2_block).await?;

        enclave.service.prove_block(request).await.map_err(NitroEnclavePoolError::Prover)
    }

    async fn acquire_enclave(
        &self,
        l2_block: u64,
    ) -> Result<(&EnclaveService, OwnedSemaphorePermit), NitroEnclavePoolError> {
        let candidate_indices: Vec<usize> = match &self.checker {
            Some(checker) => checker
                .select_all_valid_enclaves()
                .await
                .map_err(NitroEnclavePoolError::Registration)?
                .into_iter()
                .map(|v| v.index)
                .collect(),
            // Constructor guarantees at least one enclave.
            None => (0..self.enclaves.len()).collect(),
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
                NitroEnclavePoolError::Busy
            })
    }

    fn validate_registration_checker(
        &self,
        checker: &RegistrationChecker,
    ) -> Result<(), NitroEnclavePoolError> {
        let checker_transports = checker.transports();
        let first_mismatch_index = (checker_transports.len() == self.enclaves.len()).then(|| {
            self.enclaves.iter().zip(checker_transports.iter()).position(
                |(enclave, checker_transport)| !Arc::ptr_eq(&enclave.transport, checker_transport),
            )
        });

        match first_mismatch_index {
            Some(None) => Ok(()),
            Some(Some(index)) => Err(NitroEnclavePoolError::RegistrationCheckerMismatch {
                pool_transport_count: self.enclaves.len(),
                checker_transport_count: checker_transports.len(),
                first_mismatch_index: Some(index),
            }),
            None => Err(NitroEnclavePoolError::RegistrationCheckerMismatch {
                pool_transport_count: self.enclaves.len(),
                checker_transport_count: checker_transports.len(),
                first_mismatch_index: None,
            }),
        }
    }
}

/// Error returned by [`NitroEnclavePool`] proof execution.
#[derive(Debug, Error)]
pub enum NitroEnclavePoolError {
    /// Registration checker transport list does not match the pool transport list.
    #[error(
        "registration checker transports do not match pool transports: pool has \
         {pool_transport_count}, checker has {checker_transport_count}, first mismatch \
         index: {first_mismatch_index:?}"
    )]
    RegistrationCheckerMismatch {
        /// Number of transports configured in the pool.
        pool_transport_count: usize,
        /// Number of transports configured in the registration checker.
        checker_transport_count: usize,
        /// First index with a different transport when both lists have equal length.
        first_mismatch_index: Option<usize>,
    },
    /// No valid signer was available or signer validation failed.
    #[error("signer validation failed: {0}")]
    Registration(#[from] RegistrationError),
    /// Every valid enclave is already proving.
    #[error("enclave busy: another proof request is already in flight")]
    Busy,
    /// Witness generation or enclave proving failed.
    #[error(transparent)]
    Prover(#[from] ProverError<NitroBackend>),
}

impl NitroEnclavePoolError {
    /// Returns true if the pool had no available proof capacity.
    pub const fn is_busy(&self) -> bool {
        matches!(self, Self::Busy)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_genesis::ChainConfig;
    use alloy_signer::utils::public_key_to_address;
    use base_common_genesis::RollupConfig;
    use base_proof_tee_nitro_enclave::Server as EnclaveServer;
    use k256::ecdsa::VerifyingKey;

    use super::*;
    use crate::test_utils::AddressBasedMockRegistry;

    fn test_prover_config() -> ProverConfig {
        ProverConfig {
            l1_eth_url: "http://127.0.0.1:1".to_string(),
            l2_eth_url: "http://127.0.0.1:1".to_string(),
            l2_node_url: "http://127.0.0.1:1".to_string(),
            l1_beacon_url: "http://127.0.0.1:1".to_string(),
            l2_chain_id: 0,
            rollup_config: RollupConfig::default(),
            l1_config: ChainConfig::default(),
            enable_experimental_witness_endpoint: false,
        }
    }

    fn test_pool() -> NitroEnclavePool {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        NitroEnclavePool::new(test_prover_config(), transport)
    }

    async fn signer_for(transport: &NitroTransport) -> alloy_primitives::Address {
        let pk = transport.signer_public_key().await.unwrap();
        let vk = VerifyingKey::from_sec1_bytes(&pk).unwrap();
        public_key_to_address(&vk)
    }

    #[tokio::test]
    async fn prove_rejects_concurrent_request_when_permit_held() {
        let pool = test_pool();

        let permit = Arc::clone(&pool.enclaves[0].prove_permit)
            .try_acquire_owned()
            .expect("permit should be available before first acquire");

        let err = pool.prove(ProofRequest::default()).await.unwrap_err();
        assert!(matches!(err, NitroEnclavePoolError::Busy));
        assert!(err.is_busy());

        drop(permit);
        assert_eq!(pool.enclaves[0].prove_permit.available_permits(), 1);
    }

    #[tokio::test]
    async fn prove_permit_is_released_when_handle_dropped() {
        let pool = test_pool();

        let (_enclave, permit) = pool.acquire_enclave(0).await.unwrap();
        assert_eq!(pool.enclaves[0].prove_permit.available_permits(), 0);

        drop(permit);
        assert_eq!(
            pool.enclaves[0].prove_permit.available_permits(),
            MAX_CONCURRENT_PROOF_REQUESTS_PER_ENCLAVE,
            "permit must be released when the RAII handle is dropped"
        );
    }

    #[tokio::test]
    async fn prove_permit_is_per_enclave_in_multi_enclave_setup() {
        let first = Arc::new(EnclaveServer::new_local().unwrap());
        let second = Arc::new(EnclaveServer::new_local().unwrap());
        let first_transport = Arc::new(NitroTransport::local(first));
        let second_transport = Arc::new(NitroTransport::local(second));
        let pool = NitroEnclavePool::new_multi(
            test_prover_config(),
            vec![first_transport, second_transport],
        );

        let _held = Arc::clone(&pool.enclaves[0].prove_permit).try_acquire_owned().unwrap();

        assert_eq!(pool.enclaves[1].prove_permit.available_permits(), 1);
    }

    #[tokio::test]
    async fn prove_falls_through_to_second_enclave_when_first_is_busy() {
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

        let pool = NitroEnclavePool::new_multi(
            test_prover_config(),
            vec![Arc::clone(&t1), Arc::clone(&t2)],
        )
        .with_registration_checker(checker)
        .unwrap();

        let _held = Arc::clone(&pool.enclaves[0].prove_permit).try_acquire_owned().unwrap();

        let (enclave, _permit) = pool.acquire_enclave(0).await.expect("fall-through to enclave[1]");
        assert!(
            Arc::ptr_eq(&enclave.transport, &pool.enclaves[1].transport),
            "expected enclave[1] selected via fall-through"
        );
        assert_eq!(pool.enclaves[1].prove_permit.available_permits(), 0);
    }

    #[tokio::test]
    async fn prove_without_registration_checker_falls_through_when_first_is_busy() {
        let s1 = Arc::new(EnclaveServer::new_local().unwrap());
        let s2 = Arc::new(EnclaveServer::new_local().unwrap());
        let t1 = Arc::new(NitroTransport::local(s1));
        let t2 = Arc::new(NitroTransport::local(s2));
        let pool = NitroEnclavePool::new_multi(
            test_prover_config(),
            vec![Arc::clone(&t1), Arc::clone(&t2)],
        );

        let _held = Arc::clone(&pool.enclaves[0].prove_permit).try_acquire_owned().unwrap();

        let (enclave, _permit) = pool.acquire_enclave(0).await.expect("fall-through to enclave[1]");
        assert!(
            Arc::ptr_eq(&enclave.transport, &pool.enclaves[1].transport),
            "expected enclave[1] selected via fall-through without registration checker"
        );
        assert_eq!(pool.enclaves[1].prove_permit.available_permits(), 0);
    }

    #[tokio::test]
    async fn with_registration_checker_rejects_extra_checker_transport() {
        let s1 = Arc::new(EnclaveServer::new_local().unwrap());
        let s2 = Arc::new(EnclaveServer::new_local().unwrap());
        let t1 = Arc::new(NitroTransport::local(s1));
        let t2 = Arc::new(NitroTransport::local(s2));
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = Arc::new(
            RegistrationChecker::new(vec![Arc::clone(&t1), Arc::clone(&t2)], registry).unwrap(),
        );
        let pool = NitroEnclavePool::new(test_prover_config(), Arc::clone(&t1));

        let err = pool.with_registration_checker(checker).unwrap_err();
        assert!(matches!(
            err,
            NitroEnclavePoolError::RegistrationCheckerMismatch {
                pool_transport_count: 1,
                checker_transport_count: 2,
                first_mismatch_index: None,
            }
        ));
    }

    #[tokio::test]
    async fn with_registration_checker_rejects_reordered_checker_transports() {
        let s1 = Arc::new(EnclaveServer::new_local().unwrap());
        let s2 = Arc::new(EnclaveServer::new_local().unwrap());
        let t1 = Arc::new(NitroTransport::local(s1));
        let t2 = Arc::new(NitroTransport::local(s2));
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = Arc::new(
            RegistrationChecker::new(vec![Arc::clone(&t2), Arc::clone(&t1)], registry).unwrap(),
        );
        let pool = NitroEnclavePool::new_multi(
            test_prover_config(),
            vec![Arc::clone(&t1), Arc::clone(&t2)],
        );

        let err = pool.with_registration_checker(checker).unwrap_err();
        assert!(matches!(
            err,
            NitroEnclavePoolError::RegistrationCheckerMismatch {
                pool_transport_count: 2,
                checker_transport_count: 2,
                first_mismatch_index: Some(0),
            }
        ));
    }
}
