//! Shared signer-validity checker backed by the on-chain `TEEProverRegistry`.
//!
//! Two consumers, two policies:
//! - **Health endpoint** — latching: once valid, stays healthy forever (avoids
//!   ASG replacement on transient L1 failures).
//! - **Proving guard** — fail-closed with a TTL cache: rejects proof requests
//!   when the signer is invalid or L1 is unreachable.
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

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::Address;
use alloy_signer::utils::public_key_to_address;
use base_proof_contracts::TEEProverRegistryClient;
use k256::ecdsa::VerifyingKey;
use thiserror::Error;
use tokio::sync::{OnceCell, RwLock};
use tracing::warn;

use super::transport::NitroTransport;

pub(crate) const CACHE_TTL: Duration = Duration::from_secs(30);
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
}

/// Checks whether the enclave signer is a **valid** signer in the on-chain
/// `TEEProverRegistry` (registered AND matching the current image hash).
///
/// Validity results are cached for [`CACHE_TTL`] to avoid hitting L1 on every
/// request.  A separate latching flag tracks whether the signer has *ever*
/// been valid — once set, [`check_health`](Self::check_health) always succeeds.
pub struct RegistrationChecker {
    transport: Arc<NitroTransport>,
    registry: Box<dyn TEEProverRegistryClient>,
    signer: OnceCell<Address>,
    cache: RwLock<Option<(bool, Instant)>>,
    healthy: OnceCell<()>,
}

impl std::fmt::Debug for RegistrationChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrationChecker").finish_non_exhaustive()
    }
}

impl RegistrationChecker {
    /// Creates a new checker for the given transport and registry client.
    pub fn new(
        transport: Arc<NitroTransport>,
        registry: impl TEEProverRegistryClient + 'static,
    ) -> Self {
        Self {
            transport,
            registry: Box::new(registry),
            signer: OnceCell::new(),
            cache: RwLock::new(None),
            healthy: OnceCell::new(),
        }
    }

    async fn signer_address(&self) -> Result<Address, RegistrationError> {
        self.signer
            .get_or_try_init(|| async {
                let public_key = self
                    .transport
                    .signer_public_key()
                    .await
                    .map_err(|e| RegistrationError::Setup(format!("signer public key: {e}")))?;
                let verifying_key = VerifyingKey::from_sec1_bytes(&public_key)
                    .map_err(|e| RegistrationError::Setup(format!("invalid public key: {e}")))?;
                Ok(public_key_to_address(&verifying_key))
            })
            .await
            .copied()
    }

    async fn fetch_validity(&self) -> Result<(bool, Address), RegistrationError> {
        let signer = self.signer_address().await?;

        {
            let cache = self.cache.read().await;
            if let Some((valid, checked_at)) = *cache
                && checked_at.elapsed() < CACHE_TTL
            {
                return Ok((valid, signer));
            }
        }

        let result =
            tokio::time::timeout(CHECK_TIMEOUT, self.registry.is_valid_signer(signer)).await;

        match result {
            Ok(Ok(valid)) => {
                let mut cache = self.cache.write().await;
                let was_valid = cache.map(|(v, _)| v);
                *cache = Some((valid, Instant::now()));
                if !valid && was_valid != Some(false) {
                    warn!(signer = %signer, "signer is not a valid signer in TEEProverRegistry");
                }
                Ok((valid, signer))
            }
            Ok(Err(e)) => Err(RegistrationError::Rpc { signer, reason: e.to_string() }),
            Err(_) => Err(RegistrationError::Rpc { signer, reason: "request timed out".into() }),
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
        let (valid, _) = self.fetch_validity().await?;
        if valid {
            let _ = self.healthy.set(());
        }
        Ok(valid)
    }

    /// Fails the request unless the signer is currently valid.
    ///
    /// Fail-closed: if L1 is unreachable or the signer is not valid, the
    /// proof request is rejected.
    pub async fn require_valid_signer(&self) -> Result<(), RegistrationError> {
        match self.fetch_validity().await {
            Ok((true, _)) => Ok(()),
            Ok((false, signer)) => Err(RegistrationError::NotValid { signer }),
            Err(e) => Err(e),
        }
    }
}

impl RegistrationChecker {
    #[cfg(test)]
    pub(crate) fn set_signer_for_test(&self, signer: Address) {
        let _ = self.signer.set(signer);
    }

    #[cfg(test)]
    pub(crate) async fn set_cache_for_test(&self, value: Option<(bool, Instant)>) {
        *self.cache.write().await = value;
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Instant,
    };

    use alloy_primitives::{Address, address};
    use base_proof_contracts::TEEProverRegistryClient;
    use jsonrpsee::core::async_trait;

    use super::*;

    #[derive(Clone)]
    struct MockRegistry {
        valid: Arc<AtomicBool>,
        call_count: Arc<AtomicUsize>,
        should_fail: Arc<AtomicBool>,
    }

    impl MockRegistry {
        fn new(valid: bool) -> Self {
            Self {
                valid: Arc::new(AtomicBool::new(valid)),
                call_count: Arc::new(AtomicUsize::new(0)),
                should_fail: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    #[async_trait]
    impl TEEProverRegistryClient for MockRegistry {
        async fn is_valid_signer(
            &self,
            _signer: Address,
        ) -> Result<bool, base_proof_contracts::ContractError> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            if self.should_fail.load(Ordering::Relaxed) {
                return Err(base_proof_contracts::ContractError::Validation(
                    "mock RPC failure".into(),
                ));
            }
            Ok(self.valid.load(Ordering::Relaxed))
        }

        async fn is_registered_signer(
            &self,
            _signer: Address,
        ) -> Result<bool, base_proof_contracts::ContractError> {
            unimplemented!()
        }

        async fn get_registered_signers(
            &self,
        ) -> Result<Vec<Address>, base_proof_contracts::ContractError> {
            unimplemented!()
        }
    }

    const TEST_SIGNER: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    fn test_checker_with_mock(
        registry: impl TEEProverRegistryClient + 'static,
    ) -> RegistrationChecker {
        let server = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        RegistrationChecker::new(transport, registry)
    }

    fn test_checker() -> RegistrationChecker {
        let server = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let dummy_url = url::Url::parse("http://localhost:1").unwrap();
        let registry =
            base_proof_contracts::TEEProverRegistryContractClient::new(Address::ZERO, dummy_url);
        RegistrationChecker::new(transport, registry)
    }

    #[tokio::test]
    async fn health_returns_true_when_valid() {
        let checker = test_checker_with_mock(MockRegistry::new(true));
        checker.set_signer_for_test(TEST_SIGNER);
        assert!(checker.check_health().await.unwrap());
    }

    #[tokio::test]
    async fn health_returns_false_when_not_valid() {
        let checker = test_checker_with_mock(MockRegistry::new(false));
        checker.set_signer_for_test(TEST_SIGNER);
        assert!(!checker.check_health().await.unwrap());
    }

    #[tokio::test]
    async fn health_latches_after_first_success() {
        let registry = MockRegistry::new(true);
        let checker = test_checker_with_mock(registry.clone());
        checker.set_signer_for_test(TEST_SIGNER);

        assert!(checker.check_health().await.unwrap());

        registry.valid.store(false, Ordering::Relaxed);
        registry.should_fail.store(true, Ordering::Relaxed);
        checker.set_cache_for_test(None).await;

        assert!(checker.check_health().await.unwrap());
    }

    #[tokio::test]
    async fn health_errors_on_rpc_failure_before_latch() {
        let registry = MockRegistry::new(false);
        registry.should_fail.store(true, Ordering::Relaxed);
        let checker = test_checker_with_mock(registry);
        checker.set_signer_for_test(TEST_SIGNER);
        assert!(checker.check_health().await.is_err());
    }

    #[tokio::test]
    async fn health_ok_on_rpc_failure_after_latch() {
        let registry = MockRegistry::new(true);
        let checker = test_checker_with_mock(registry.clone());
        checker.set_signer_for_test(TEST_SIGNER);
        assert!(checker.check_health().await.unwrap());

        registry.should_fail.store(true, Ordering::Relaxed);
        checker.set_cache_for_test(None).await;
        assert!(checker.check_health().await.unwrap());
    }

    #[tokio::test]
    async fn require_valid_signer_ok_when_cached_valid() {
        let checker = test_checker();
        checker.set_signer_for_test(TEST_SIGNER);
        checker.set_cache_for_test(Some((true, Instant::now()))).await;
        assert!(checker.require_valid_signer().await.is_ok());
    }

    #[tokio::test]
    async fn require_valid_signer_rejects_when_cached_invalid() {
        let checker = test_checker();
        checker.set_signer_for_test(TEST_SIGNER);
        checker.set_cache_for_test(Some((false, Instant::now()))).await;
        assert!(matches!(
            checker.require_valid_signer().await.unwrap_err(),
            RegistrationError::NotValid { .. }
        ));
    }

    #[tokio::test]
    async fn require_valid_signer_rejects_on_rpc_error() {
        let checker = test_checker();
        checker.set_signer_for_test(TEST_SIGNER);
        assert!(matches!(
            checker.require_valid_signer().await.unwrap_err(),
            RegistrationError::Rpc { .. }
        ));
    }

    #[tokio::test]
    async fn require_valid_signer_rejects_on_expired_cache() {
        let checker = test_checker();
        checker.set_signer_for_test(TEST_SIGNER);
        let expired = Instant::now() - CACHE_TTL - Duration::from_secs(1);
        checker.set_cache_for_test(Some((true, expired))).await;
        assert!(matches!(
            checker.require_valid_signer().await.unwrap_err(),
            RegistrationError::Rpc { .. }
        ));
    }

    #[tokio::test]
    async fn cache_hit_within_ttl() {
        let registry = MockRegistry::new(true);
        let call_count = Arc::clone(&registry.call_count);
        let checker = test_checker_with_mock(registry);
        checker.set_signer_for_test(TEST_SIGNER);

        assert!(checker.require_valid_signer().await.is_ok());
        assert_eq!(call_count.load(Ordering::Relaxed), 1);

        assert!(checker.require_valid_signer().await.is_ok());
        assert_eq!(call_count.load(Ordering::Relaxed), 1);
    }
}
