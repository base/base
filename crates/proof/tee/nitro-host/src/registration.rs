//! Shared signer-validity checker backed by the on-chain `TEEProverRegistry`.
//!
//! Two consumers, two policies:
//! - **Health endpoint** — latching: once valid, stays healthy forever (avoids
//!   ASG replacement on transient L1 failures).
//! - **Proving guard** — fail-closed: rejects proof requests when the signer
//!   is invalid or L1 is unreachable.
//!
//! # Trade-off: latching health after deregistration
//!
//! After a signer deregistration or image rotation the health latch stays set
//! while the proving guard rejects every request.  The prover will continue
//! receiving traffic from the load balancer (because `/healthz` returns 200)
//! but respond with `-32001` errors.  This is intentional: the ASG must not
//! terminate the instance on a transient L1 blip, and proof-request callers
//! already retry on other nodes.  If `/healthz` is ever the **sole** LB
//! signal with no retry layer, switch to a bounded latch (e.g. stay healthy
//! for N minutes after the last successful validation).

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_signer::utils::public_key_to_address;
use base_proof_contracts::TEEProverRegistryClient;
use k256::ecdsa::VerifyingKey;
use thiserror::Error;
use tokio::sync::OnceCell;
use tracing::warn;

use super::transport::NitroTransport;

const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors from signer-validity checks.
#[derive(Debug, Error)]
pub enum RegistrationError {
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

/// A signer that has been confirmed valid on-chain via `TEEProverRegistry`.
#[derive(Debug)]
pub struct ValidSigner {
    /// Index of the enclave in the configured transport list.
    pub index: usize,
    /// Ethereum address of the valid signer.
    pub signer: Address,
}

/// Checks whether the enclave signer is a **valid** signer in the on-chain
/// `TEEProverRegistry` (registered AND matching the current image hash).
///
/// Each call to [`require_valid_signer`](Self::require_valid_signer) performs a
/// live L1 query — proof requests are infrequent enough that caching is
/// unnecessary.  A separate latching flag tracks whether the signer has *ever*
/// been valid — once set, [`check_health`](Self::check_health) always succeeds.
pub struct RegistrationChecker {
    transports: Vec<Arc<NitroTransport>>,
    registry: Box<dyn TEEProverRegistryClient>,
    healthy: OnceCell<()>,
}

impl std::fmt::Debug for RegistrationChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrationChecker").finish_non_exhaustive()
    }
}

impl RegistrationChecker {
    /// Creates a new checker for the given transports and registry client.
    ///
    /// Returns an error if `transports` is empty.
    pub fn new(
        transports: Vec<Arc<NitroTransport>>,
        registry: impl TEEProverRegistryClient + 'static,
    ) -> Result<Self, RegistrationError> {
        if transports.is_empty() {
            return Err(RegistrationError::Setup("at least one transport is required".into()));
        }
        Ok(Self { transports, registry: Box::new(registry), healthy: OnceCell::new() })
    }

    async fn signer_address(transport: &NitroTransport) -> Result<Address, RegistrationError> {
        let public_key = transport
            .signer_public_key()
            .await
            .map_err(|e| RegistrationError::Setup(format!("signer public key: {e}")))?;
        let verifying_key = VerifyingKey::from_sec1_bytes(&public_key)
            .map_err(|e| RegistrationError::Setup(format!("invalid public key: {e}")))?;
        Ok(public_key_to_address(&verifying_key))
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

    async fn fetch_validity(&self) -> Result<bool, RegistrationError> {
        let mut first_rpc_error = None;

        for (index, transport) in self.transports.iter().enumerate() {
            let signer = match Self::signer_address(transport).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, index, "skipping transport: key fetch failed");
                    continue;
                }
            };

            match self.is_valid_signer(signer).await {
                Ok(true) => return Ok(true),
                Ok(false) => {
                    warn!(signer = %signer, index, "signer not valid in TEEProverRegistry");
                }
                Err(e) => {
                    first_rpc_error.get_or_insert(e);
                }
            };
        }

        match first_rpc_error {
            Some(e) => Err(e),
            None => Ok(false),
        }
    }

    /// Latching health check: returns `true` once the signer has ever been
    /// confirmed valid, and stays `true` forever after — even if the signer
    /// is later deregistered.  See the [module-level docs](self) for the
    /// trade-off this implies.
    pub async fn check_health(&self) -> Result<bool, RegistrationError> {
        if self.healthy.get().is_some() {
            return Ok(true);
        }
        let valid = self.fetch_validity().await?;
        if valid {
            let _ = self.healthy.set(());
        }
        Ok(valid)
    }

    /// Fails the request unless the **first** transport's signer is currently
    /// valid.
    ///
    /// Fail-closed: if L1 is unreachable or the signer is not valid, the
    /// proof request is rejected.
    pub async fn require_valid_signer(&self) -> Result<(), RegistrationError> {
        // Constructor guarantees at least one transport.
        let signer = Self::signer_address(&self.transports[0]).await?;

        match self.is_valid_signer(signer).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(RegistrationError::NotValid { signer }),
            Err(e) => Err(e),
        }
    }

    /// Selects the first enclave whose signer is currently valid on-chain.
    ///
    /// Returns as soon as a valid signer is found (config order).
    pub async fn select_valid_enclave(&self) -> Result<ValidSigner, RegistrationError> {
        let mut discovered = Vec::new();

        for (index, transport) in self.transports.iter().enumerate() {
            let signer = match Self::signer_address(transport).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, index, "skipping transport: key fetch failed");
                    continue;
                }
            };

            discovered.push(signer);

            match self.is_valid_signer(signer).await {
                Ok(true) => return Ok(ValidSigner { index, signer }),
                Ok(false) => {
                    warn!(signer = %signer, index, "signer not valid in TEEProverRegistry");
                }
                Err(e) => return Err(e),
            }
        }

        Err(RegistrationError::NoValidSigner { signers: discovered })
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
        let transport = Arc::new(NitroTransport::local(server));
        RegistrationChecker::new(vec![transport], registry).unwrap()
    }

    fn test_checker() -> RegistrationChecker {
        let server = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let dummy_url = url::Url::parse("http://localhost:1").unwrap();
        let registry = base_proof_contracts::TEEProverRegistryContractClient::new(
            alloy_primitives::Address::ZERO,
            dummy_url,
        );
        RegistrationChecker::new(vec![transport], registry).unwrap()
    }

    fn two_transport_checker(
        registry: impl TEEProverRegistryClient + 'static,
    ) -> RegistrationChecker {
        let s1 = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let s2 = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let t1 = Arc::new(NitroTransport::local(s1));
        let t2 = Arc::new(NitroTransport::local(s2));
        RegistrationChecker::new(vec![t1, t2], registry).unwrap()
    }

    async fn transport_signer_address(transport: &NitroTransport) -> Address {
        let pk = transport.signer_public_key().await.unwrap();
        let vk = VerifyingKey::from_sec1_bytes(&pk).unwrap();
        public_key_to_address(&vk)
    }

    async fn two_transport_signers(checker: &RegistrationChecker) -> (Address, Address) {
        let a = transport_signer_address(&checker.transports[0]).await;
        let b = transport_signer_address(&checker.transports[1]).await;
        (a, b)
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
    async fn select_first_invalid_second_valid_returns_index_1() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_transport_checker(registry.clone());
        let (addr0, addr1) = two_transport_signers(&checker).await;

        registry.validity_map.lock().unwrap().insert(addr0, false);
        registry.validity_map.lock().unwrap().insert(addr1, true);

        let valid = checker.select_valid_enclave().await.unwrap();
        assert_eq!(valid.index, 1);
        assert_eq!(valid.signer, addr1);
    }

    #[tokio::test]
    async fn select_both_valid_returns_first() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_transport_checker(registry.clone());
        let (addr0, addr1) = two_transport_signers(&checker).await;

        registry.validity_map.lock().unwrap().insert(addr0, true);
        registry.validity_map.lock().unwrap().insert(addr1, true);

        let valid = checker.select_valid_enclave().await.unwrap();
        assert_eq!(valid.index, 0);
        assert_eq!(valid.signer, addr0);
    }

    #[tokio::test]
    async fn select_none_valid_returns_no_valid_signer() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_transport_checker(registry);

        let err = checker.select_valid_enclave().await.unwrap_err();
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
        let checker = two_transport_checker(registry.clone());
        let (addr0, addr1) = two_transport_signers(&checker).await;

        registry.validity_map.lock().unwrap().insert(addr0, false);
        registry.validity_map.lock().unwrap().insert(addr1, true);

        assert!(checker.check_health().await.unwrap());
    }

    #[tokio::test]
    async fn health_latches_with_multi_transport() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_transport_checker(registry.clone());
        let (addr0, addr1) = two_transport_signers(&checker).await;

        registry.validity_map.lock().unwrap().insert(addr0, true);
        registry.validity_map.lock().unwrap().insert(addr1, false);

        assert!(checker.check_health().await.unwrap());

        registry.validity_map.lock().unwrap().insert(addr0, false);
        registry.should_fail.store(true, Ordering::Relaxed);

        assert!(checker.check_health().await.unwrap());
    }

    #[tokio::test]
    async fn require_valid_signer_checks_first_transport_only() {
        let registry = AddressBasedMockRegistry::new(HashMap::new());
        let checker = two_transport_checker(registry.clone());
        let (addr0, addr1) = two_transport_signers(&checker).await;

        registry.validity_map.lock().unwrap().insert(addr0, false);
        registry.validity_map.lock().unwrap().insert(addr1, true);

        assert!(matches!(
            checker.require_valid_signer().await.unwrap_err(),
            RegistrationError::NotValid { .. }
        ));
    }
}
