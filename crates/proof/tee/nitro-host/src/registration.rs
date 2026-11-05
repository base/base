//! Shared signer checker backed by the onchain `TEEProverRegistry`.
//!
//! Health accepts a currently valid signer or a registered image hash matching
//! the configured nonzero pending hash. Once healthy, it stays healthy forever
//! to avoid ASG replacement on transient L1 failures. Proving and enclave routing
//! still require current onchain validity and never accept the pending hash.
//!
//! # Trade-off: latching health after deregistration
//!
//! After a signer deregistration or image rotation the health latch stays set
//! while the proving guard rejects every request.  The prover will continue
//! returning 200 from `/healthz` but respond with `-32001` errors. The
//! registrar probes `/readyz` (not registration-gated) so bootstrap stays
//! independent of this latch. If another deployment uses `/healthz` as its
//! **sole** load-balancer signal with no retry layer, switch to a bounded latch
//! (e.g. stay healthy for N minutes after the last successful validation).

use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, B256};
use base_proof_contracts::{TEEProverRegistryClient, TEEProverRegistryContractClient};
use base_proof_tee_nitro_enclave::EnclaveClient;
use thiserror::Error;
use tokio::sync::OnceCell;
use tracing::warn;

use super::health::RegistrationHealthConfig;

const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors from signer-validity checks.
#[derive(Debug, Error)]
pub enum RegistrationError {
    /// L1 RPC URL could not be parsed.
    #[error("invalid L1 RPC URL {url}: {reason}")]
    InvalidRpcUrl {
        /// Invalid L1 RPC URL.
        url: String,
        /// Parse failure details.
        reason: String,
    },
    /// Enclave signer key could not be retrieved or parsed.
    #[error("signer setup failed: {0}")]
    Setup(String),
    /// L1 RPC call failed or timed out.
    #[error("L1 RPC failed for signer {signer}: {reason}")]
    Rpc {
        /// The signer address that was being checked.
        signer: Address,
        /// The underlying error message.
        reason: String,
    },
    /// The signer is not a valid signer in `TEEProverRegistry`.
    #[error("signer {signer} is not a valid signer in TEEProverRegistry")]
    NotValid {
        /// The signer address that failed validation.
        signer: Address,
    },
    /// No enclave signer is currently valid in `TEEProverRegistry`.
    #[error("no valid signer found among: {signers:?}")]
    NoValidSigner {
        /// The signer addresses that were checked.
        signers: Vec<Address>,
    },
}

/// A signer that has been confirmed valid onchain via `TEEProverRegistry`.
#[derive(Debug)]
pub struct ValidSigner {
    /// Index of the enclave in the configured enclave client list.
    pub index: usize,
    /// Ethereum address of the valid signer.
    pub signer: Address,
}

/// Checks enclave signers against the onchain `TEEProverRegistry`.
///
/// Proving and routing perform live validity checks against the current image
/// hash. Health also accepts a registered image matching an optional pending
/// hash, so the next image can deploy before onchain activation. Once either
/// health condition succeeds, [`check_health`](Self::check_health) stays healthy
/// without further registry calls.
pub struct RegistrationChecker {
    enclave_clients: Vec<Arc<EnclaveClient>>,
    registry: Box<dyn TEEProverRegistryClient>,
    healthy: OnceCell<()>,
    pending_image_hash: Option<B256>,
}

impl std::fmt::Debug for RegistrationChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrationChecker").finish_non_exhaustive()
    }
}

impl RegistrationChecker {
    /// Creates a new checker for the given enclave clients and registry client.
    ///
    /// Returns an error if `enclave_clients` is empty.
    pub fn new(
        enclave_clients: Vec<Arc<EnclaveClient>>,
        registry: impl TEEProverRegistryClient + 'static,
    ) -> Result<Self, RegistrationError> {
        if enclave_clients.is_empty() {
            return Err(RegistrationError::Setup("at least one enclave client is required".into()));
        }
        Ok(Self {
            enclave_clients,
            registry: Box::new(registry),
            healthy: OnceCell::new(),
            pending_image_hash: None,
        })
    }

    /// Configures an optional image hash accepted by health checks, but not proving.
    ///
    /// Returns [`RegistrationError::Setup`] if the pending hash is zero.
    pub fn with_pending_image_hash(
        mut self,
        pending_image_hash: Option<B256>,
    ) -> Result<Self, RegistrationError> {
        if pending_image_hash.is_some_and(|hash| hash.is_zero()) {
            return Err(RegistrationError::Setup("pending TEE image hash must be nonzero".into()));
        }
        self.pending_image_hash = pending_image_hash;
        Ok(self)
    }

    /// Creates a checker from a deployed `TEEProverRegistry` configuration.
    pub fn from_health_config(
        enclave_clients: Vec<Arc<EnclaveClient>>,
        config: &RegistrationHealthConfig,
    ) -> Result<Self, RegistrationError> {
        let l1_url = url::Url::parse(&config.l1_rpc_url).map_err(|err| {
            RegistrationError::InvalidRpcUrl {
                url: config.l1_rpc_url.clone(),
                reason: err.to_string(),
            }
        })?;
        let registry = TEEProverRegistryContractClient::new(config.registry_address, l1_url);

        Self::new(enclave_clients, registry)?.with_pending_image_hash(config.pending_image_hash)
    }

    /// Returns the configured enclave clients in checker index order.
    pub fn enclave_clients(&self) -> Vec<Arc<EnclaveClient>> {
        self.enclave_clients.iter().map(Arc::clone).collect()
    }

    async fn signer_address(enclave_client: &EnclaveClient) -> Result<Address, RegistrationError> {
        enclave_client
            .signer_address()
            .await
            .map_err(|e| RegistrationError::Setup(format!("signer public key: {e}")))
    }

    async fn is_valid_signer(&self, signer: Address) -> Result<bool, RegistrationError> {
        let result =
            tokio::time::timeout(CHECK_TIMEOUT, self.registry.is_valid_signer(signer)).await;

        match result {
            Ok(Ok(valid)) => Ok(valid),
            Ok(Err(e)) => Err(RegistrationError::Rpc { signer, reason: e.to_string() }),
            Err(_) => Err(RegistrationError::Rpc { signer, reason: "request timed out".into() }),
        }
    }

    async fn fetch_health(&self) -> Result<bool, RegistrationError> {
        let mut first_rpc_error = None;

        for (index, enclave_client) in self.enclave_clients.iter().enumerate() {
            let signer = match Self::signer_address(enclave_client).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, index, "skipping enclave client: key fetch failed");
                    continue;
                }
            };

            match self.is_valid_signer(signer).await {
                Ok(true) => return Ok(true),
                Ok(false) => {
                    if let Some(pending) = self.pending_image_hash {
                        match tokio::time::timeout(
                            CHECK_TIMEOUT,
                            self.registry.signer_image_hash(signer),
                        )
                        .await
                        {
                            Ok(Ok(image_hash)) if image_hash == pending => return Ok(true),
                            Ok(Ok(_)) => {}
                            Ok(Err(e)) => {
                                first_rpc_error.get_or_insert_with(|| RegistrationError::Rpc {
                                    signer,
                                    reason: e.to_string(),
                                });
                            }
                            Err(_) => {
                                first_rpc_error.get_or_insert_with(|| RegistrationError::Rpc {
                                    signer,
                                    reason: "request timed out".into(),
                                });
                            }
                        }
                    }
                    warn!(signer = %signer, index, "signer does not satisfy TEE registry health policy");
                }
                Err(e) => {
                    first_rpc_error.get_or_insert(e);
                }
            };
        }

        first_rpc_error.map_or(Ok(false), Err)
    }

    /// Returns `true` once any signer is valid or has the configured pending image.
    ///
    /// Success latches forever, even after deregistration. Before the latch,
    /// pending image reads run only for invalid signers, at most once per
    /// enclave client per call, with the same timeout as validity reads. An RPC
    /// failure returns an error if no other signer satisfies the health policy.
    /// See the [module-level docs](self) for the latch trade-off.
    pub async fn check_health(&self) -> Result<bool, RegistrationError> {
        if self.healthy.get().is_some() {
            return Ok(true);
        }
        let valid = self.fetch_health().await?;
        if valid {
            let _ = self.healthy.set(());
        }
        Ok(valid)
    }

    /// Fails the request unless the **first** enclave client's signer is currently
    /// valid.
    ///
    /// Fail-closed: if L1 is unreachable or the signer is not valid, the
    /// proof request is rejected.
    pub async fn require_valid_signer(&self) -> Result<(), RegistrationError> {
        // Constructor guarantees at least one enclave client.
        let signer = Self::signer_address(&self.enclave_clients[0]).await?;

        match self.is_valid_signer(signer).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(RegistrationError::NotValid { signer }),
            Err(e) => Err(e),
        }
    }

    /// Returns every enclave whose signer is currently valid onchain, in
    /// config order.
    ///
    /// Best-effort across enclaves: RPC errors for individual signers are
    /// logged and skipped rather than failing the whole call. If no valid
    /// signer was found, returns the first RPC error encountered (so the
    /// caller surfaces L1 reachability problems) or
    /// [`RegistrationError::NoValidSigner`] if every signer was reachable
    /// but invalid.
    pub async fn select_all_valid_enclaves(&self) -> Result<Vec<ValidSigner>, RegistrationError> {
        let mut valid = Vec::new();
        let mut discovered = Vec::new();
        let mut first_rpc_error = None;

        for (index, enclave_client) in self.enclave_clients.iter().enumerate() {
            let signer = match Self::signer_address(enclave_client).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, index, "skipping enclave client: key fetch failed");
                    continue;
                }
            };

            discovered.push(signer);

            match self.is_valid_signer(signer).await {
                Ok(true) => valid.push(ValidSigner { index, signer }),
                Ok(false) => {
                    warn!(signer = %signer, index, "signer not valid in TEEProverRegistry");
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        signer = %signer,
                        index,
                        "L1 RPC failed during validity check, skipping enclave"
                    );
                    first_rpc_error.get_or_insert(e);
                }
            }
        }

        if !valid.is_empty() {
            Ok(valid)
        } else if let Some(e) = first_rpc_error {
            Err(e)
        } else {
            Err(RegistrationError::NoValidSigner { signers: discovered })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, atomic::Ordering},
    };

    use base_proof_contracts::TEEProverRegistryClient;

    use super::*;
    use crate::test_utils::{AddressBasedMockRegistry, MockRegistry};

    fn test_checker_with_mock(
        registry: impl TEEProverRegistryClient + 'static,
    ) -> RegistrationChecker {
        let server = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let enclave_client = Arc::new(EnclaveClient::in_process(server));
        RegistrationChecker::new(vec![enclave_client], registry).unwrap()
    }

    fn test_checker() -> RegistrationChecker {
        let server = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let enclave_client = Arc::new(EnclaveClient::in_process(server));
        let dummy_url = url::Url::parse("http://localhost:1").unwrap();
        let registry = base_proof_contracts::TEEProverRegistryContractClient::new(
            alloy_primitives::Address::ZERO,
            dummy_url,
        );
        RegistrationChecker::new(vec![enclave_client], registry).unwrap()
    }

    fn two_enclave_client_checker(
        registry: impl TEEProverRegistryClient + 'static,
    ) -> RegistrationChecker {
        let s1 = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let s2 = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let t1 = Arc::new(EnclaveClient::in_process(s1));
        let t2 = Arc::new(EnclaveClient::in_process(s2));
        RegistrationChecker::new(vec![t1, t2], registry).unwrap()
    }

    async fn enclave_client_signer_address(enclave_client: &EnclaveClient) -> Address {
        enclave_client.signer_address().await.unwrap()
    }

    async fn two_enclave_client_signers(checker: &RegistrationChecker) -> (Address, Address) {
        let a = enclave_client_signer_address(&checker.enclave_clients[0]).await;
        let b = enclave_client_signer_address(&checker.enclave_clients[1]).await;
        (a, b)
    }

    #[tokio::test]
    async fn pending_health_accepts_only_the_configured_image() {
        let pending = B256::repeat_byte(1);
        let registry = MockRegistry::new(false);
        let checker = test_checker_with_mock(registry.clone())
            .with_pending_image_hash(Some(pending))
            .unwrap();

        for image_hash in [B256::ZERO, B256::repeat_byte(2)] {
            *registry.image_hash.lock().unwrap() = image_hash;
            assert!(!checker.check_health().await.unwrap());
        }

        assert_eq!(registry.image_hash_call_count.load(Ordering::Relaxed), 2);
        *registry.image_hash.lock().unwrap() = pending;
        assert!(checker.check_health().await.unwrap());
        assert!(checker.check_health().await.unwrap());
        assert_eq!(registry.image_hash_call_count.load(Ordering::Relaxed), 3);
        assert_eq!(registry.call_count.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn no_pending_hash_keeps_invalid_signers_unhealthy_without_image_reads() {
        let registry = MockRegistry::new(false);
        *registry.image_hash.lock().unwrap() = B256::repeat_byte(1);
        registry.image_hash_should_fail.store(true, Ordering::Relaxed);
        let checker = test_checker_with_mock(registry.clone());

        assert!(!checker.check_health().await.unwrap());
        assert!(!checker.check_health().await.unwrap());
        assert_eq!(registry.call_count.load(Ordering::Relaxed), 2);
        assert_eq!(registry.image_hash_call_count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn pending_health_latch_does_not_authorize_proving_or_routing() {
        let pending = B256::repeat_byte(1);
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_enclave_client_checker(registry.clone())
            .with_pending_image_hash(Some(pending))
            .unwrap();
        let (pending_signer, current_signer) = two_enclave_client_signers(&checker).await;
        registry.image_hashes.lock().unwrap().insert(pending_signer, pending);

        assert!(checker.check_health().await.unwrap());
        assert!(matches!(checker.require_valid_signer().await,
            Err(RegistrationError::NotValid { signer }) if signer == pending_signer));
        assert!(matches!(
            checker.select_all_valid_enclaves().await,
            Err(RegistrationError::NoValidSigner { .. })
        ));

        registry.validity_map.lock().unwrap().insert(current_signer, true);
        let selected = checker.select_all_valid_enclaves().await.unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].signer, current_signer);
        assert_eq!(selected[0].index, 1);
        assert!(checker.check_health().await.unwrap());
        assert_eq!(*registry.image_hash_calls.lock().unwrap(), vec![pending_signer]);
    }

    #[tokio::test]
    async fn pending_health_reads_each_invalid_enclave_client_once_per_poll_before_latch() {
        let pending = B256::repeat_byte(1);
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_enclave_client_checker(registry.clone())
            .with_pending_image_hash(Some(pending))
            .unwrap();
        let (a, b) = two_enclave_client_signers(&checker).await;

        assert!(!checker.check_health().await.unwrap());
        assert_eq!(*registry.image_hash_calls.lock().unwrap(), vec![a, b]);
        assert!(!checker.check_health().await.unwrap());
        assert_eq!(*registry.image_hash_calls.lock().unwrap(), vec![a, b, a, b]);
        registry.image_hashes.lock().unwrap().insert(b, pending);
        assert!(checker.check_health().await.unwrap());
        assert_eq!(*registry.image_hash_calls.lock().unwrap(), vec![a, b, a, b, a, b]);
        assert_eq!(registry.call_count.load(Ordering::Relaxed), 6);

        registry.should_fail.store(true, Ordering::Relaxed);
        registry.image_hash_fail_signers.lock().unwrap().extend([a, b]);
        assert!(checker.check_health().await.unwrap());
        assert_eq!(registry.call_count.load(Ordering::Relaxed), 6);
        assert_eq!(registry.image_hash_calls.lock().unwrap().len(), 6);
    }

    #[tokio::test]
    async fn pending_health_skips_image_reads_for_valid_signers_and_validity_errors() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_enclave_client_checker(registry.clone())
            .with_pending_image_hash(Some(B256::repeat_byte(1)))
            .unwrap();
        let (a, b) = two_enclave_client_signers(&checker).await;
        registry.fail_signers.lock().unwrap().insert(a);
        registry.validity_map.lock().unwrap().insert(b, true);

        assert!(checker.check_health().await.unwrap());
        assert_eq!(registry.call_count.load(Ordering::Relaxed), 2);
        assert!(registry.image_hash_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pending_health_image_rpc_failure_is_an_error_before_latch() {
        let registry = MockRegistry::new(false);
        registry.image_hash_should_fail.store(true, Ordering::Relaxed);
        let checker = test_checker_with_mock(registry)
            .with_pending_image_hash(Some(B256::repeat_byte(1)))
            .unwrap();

        let error = checker.check_health().await.unwrap_err();
        assert!(matches!(error, RegistrationError::Rpc { .. }));
        assert!(error.to_string().contains("mock image hash RPC failure"));
    }

    #[tokio::test(start_paused = true)]
    async fn pending_health_image_rpc_timeout_is_an_error_before_latch() {
        let registry = MockRegistry::new(false);
        registry.image_hash_should_stall.store(true, Ordering::Relaxed);
        let checker = test_checker_with_mock(registry)
            .with_pending_image_hash(Some(B256::repeat_byte(1)))
            .unwrap();

        let result =
            tokio::time::timeout(CHECK_TIMEOUT + Duration::from_secs(1), checker.check_health())
                .await
                .expect("image hash read exceeded the registry check timeout");
        assert!(matches!(result, Err(RegistrationError::Rpc { .. })));
    }

    #[tokio::test]
    async fn pending_health_can_recover_from_one_image_rpc_failure() {
        let pending = B256::repeat_byte(1);
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_enclave_client_checker(registry.clone())
            .with_pending_image_hash(Some(pending))
            .unwrap();
        let (a, b) = two_enclave_client_signers(&checker).await;
        registry.image_hash_fail_signers.lock().unwrap().insert(a);
        registry.image_hashes.lock().unwrap().insert(b, pending);

        assert!(checker.check_health().await.unwrap());
        assert_eq!(*registry.image_hash_calls.lock().unwrap(), vec![a, b]);
    }

    #[test]
    fn pending_health_rejects_zero_hash() {
        assert!(
            test_checker_with_mock(MockRegistry::new(false))
                .with_pending_image_hash(Some(B256::ZERO))
                .is_err()
        );
    }

    #[tokio::test]
    async fn health_returns_true_when_valid() {
        let checker = test_checker_with_mock(MockRegistry::new(true));
        assert!(checker.check_health().await.unwrap());
    }

    #[tokio::test]
    async fn health_returns_false_when_not_valid() {
        let checker = test_checker_with_mock(MockRegistry::new(false));
        assert!(!checker.check_health().await.unwrap());
    }

    #[tokio::test]
    async fn health_latches_after_first_success() {
        let registry = MockRegistry::new(true);
        let checker = test_checker_with_mock(registry.clone());

        assert!(checker.check_health().await.unwrap());

        registry.valid.store(false, Ordering::Relaxed);
        registry.should_fail.store(true, Ordering::Relaxed);

        assert!(checker.check_health().await.unwrap());
    }

    #[tokio::test]
    async fn health_errors_on_rpc_failure_before_latch() {
        let registry = MockRegistry::new(false);
        registry.should_fail.store(true, Ordering::Relaxed);
        let checker = test_checker_with_mock(registry);
        assert!(checker.check_health().await.is_err());
    }

    #[tokio::test]
    async fn health_ok_on_rpc_failure_after_latch() {
        let registry = MockRegistry::new(true);
        let checker = test_checker_with_mock(registry.clone());
        assert!(checker.check_health().await.unwrap());

        registry.should_fail.store(true, Ordering::Relaxed);
        assert!(checker.check_health().await.unwrap());
    }

    #[tokio::test]
    async fn require_valid_signer_ok() {
        let checker = test_checker_with_mock(MockRegistry::new(true));
        assert!(checker.require_valid_signer().await.is_ok());
    }

    #[tokio::test]
    async fn require_valid_signer_rejects_when_invalid() {
        let checker = test_checker_with_mock(MockRegistry::new(false));
        assert!(matches!(
            checker.require_valid_signer().await.unwrap_err(),
            RegistrationError::NotValid { .. }
        ));
    }

    #[tokio::test]
    async fn require_valid_signer_rejects_on_rpc_error() {
        let checker = test_checker();
        assert!(matches!(
            checker.require_valid_signer().await.unwrap_err(),
            RegistrationError::Rpc { .. }
        ));
    }

    #[tokio::test]
    async fn each_call_hits_registry() {
        let registry = MockRegistry::new(true);
        let call_count = Arc::clone(&registry.call_count);
        let checker = test_checker_with_mock(registry);

        assert!(checker.require_valid_signer().await.is_ok());
        assert_eq!(call_count.load(Ordering::Relaxed), 1);

        assert!(checker.require_valid_signer().await.is_ok());
        assert_eq!(call_count.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn select_all_skips_invalid_returns_only_valid() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_enclave_client_checker(registry.clone());
        let (addr0, addr1) = two_enclave_client_signers(&checker).await;

        registry.validity_map.lock().unwrap().insert(addr0, false);
        registry.validity_map.lock().unwrap().insert(addr1, true);

        let valid = checker.select_all_valid_enclaves().await.unwrap();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].index, 1);
        assert_eq!(valid[0].signer, addr1);
    }

    #[tokio::test]
    async fn select_all_both_valid_returns_in_config_order() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_enclave_client_checker(registry.clone());
        let (addr0, addr1) = two_enclave_client_signers(&checker).await;

        registry.validity_map.lock().unwrap().insert(addr0, true);
        registry.validity_map.lock().unwrap().insert(addr1, true);

        let valid = checker.select_all_valid_enclaves().await.unwrap();
        assert_eq!(valid.len(), 2);
        assert_eq!(valid[0].index, 0);
        assert_eq!(valid[0].signer, addr0);
        assert_eq!(valid[1].index, 1);
        assert_eq!(valid[1].signer, addr1);
    }

    #[tokio::test]
    async fn select_all_none_valid_returns_no_valid_signer() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_enclave_client_checker(registry);

        let err = checker.select_all_valid_enclaves().await.unwrap_err();
        match err {
            RegistrationError::NoValidSigner { signers } => {
                assert_eq!(signers.len(), 2);
            }
            other => panic!("expected NoValidSigner, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn health_any_valid_returns_true() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_enclave_client_checker(registry.clone());
        let (addr0, addr1) = two_enclave_client_signers(&checker).await;

        registry.validity_map.lock().unwrap().insert(addr0, false);
        registry.validity_map.lock().unwrap().insert(addr1, true);

        assert!(checker.check_health().await.unwrap());
    }

    #[tokio::test]
    async fn health_latches_with_multi_enclave_client() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_enclave_client_checker(registry.clone());
        let (addr0, addr1) = two_enclave_client_signers(&checker).await;

        registry.validity_map.lock().unwrap().insert(addr0, true);
        registry.validity_map.lock().unwrap().insert(addr1, false);

        assert!(checker.check_health().await.unwrap());

        registry.validity_map.lock().unwrap().insert(addr0, false);
        registry.should_fail.store(true, Ordering::Relaxed);

        assert!(checker.check_health().await.unwrap());
    }

    #[tokio::test]
    async fn select_all_valid_returns_valid_when_other_rpc_fails() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_enclave_client_checker(registry.clone());
        let (addr0, addr1) = two_enclave_client_signers(&checker).await;

        registry.validity_map.lock().unwrap().insert(addr0, true);
        registry.validity_map.lock().unwrap().insert(addr1, true);
        // Enclave 1's validity check will fail at the RPC layer; enclave 0
        // is still valid and must be returned.
        registry.fail_signers.lock().unwrap().insert(addr1);

        let valid = checker.select_all_valid_enclaves().await.unwrap();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].index, 0);
        assert_eq!(valid[0].signer, addr0);
    }

    #[tokio::test]
    async fn select_all_valid_propagates_rpc_error_when_none_valid() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_enclave_client_checker(registry.clone());
        let (addr0, addr1) = two_enclave_client_signers(&checker).await;

        // Enclave 0 RPC fails, enclave 1 reports not-valid. No valid signer,
        // so the RPC error should surface so the operator sees L1 trouble.
        registry.fail_signers.lock().unwrap().insert(addr0);
        registry.validity_map.lock().unwrap().insert(addr1, false);

        let err = checker.select_all_valid_enclaves().await.unwrap_err();
        assert!(matches!(err, RegistrationError::Rpc { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn require_valid_signer_checks_first_enclave_client_only() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_enclave_client_checker(registry.clone());
        let (addr0, addr1) = two_enclave_client_signers(&checker).await;

        registry.validity_map.lock().unwrap().insert(addr0, false);
        registry.validity_map.lock().unwrap().insert(addr1, true);

        assert!(matches!(
            checker.require_valid_signer().await.unwrap_err(),
            RegistrationError::NotValid { .. }
        ));
    }
}
