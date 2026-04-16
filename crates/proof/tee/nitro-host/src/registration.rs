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
}

/// Checks whether the enclave signer is a **valid** signer in the on-chain
/// `TEEProverRegistry` (registered AND matching the current image hash).
///
/// Each call to [`require_valid_signer`](Self::require_valid_signer) performs a
/// live L1 query — proof requests are infrequent enough that caching is
/// unnecessary.  A separate latching flag tracks whether the signer has *ever*
/// been valid — once set, [`check_health`](Self::check_health) always succeeds.
pub struct RegistrationChecker {
    transport: Arc<NitroTransport>,
    registry: Box<dyn TEEProverRegistryClient>,
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
        Self { transport, registry: Box::new(registry), healthy: OnceCell::new() }
    }

    async fn signer_address(&self) -> Result<Address, RegistrationError> {
        let public_key = self
            .transport
            .signer_public_key()
            .await
            .map_err(|e| RegistrationError::Setup(format!("signer public key: {e}")))?;
        let verifying_key = VerifyingKey::from_sec1_bytes(&public_key)
            .map_err(|e| RegistrationError::Setup(format!("invalid public key: {e}")))?;
        Ok(public_key_to_address(&verifying_key))
    }

    async fn fetch_validity(&self) -> Result<(bool, Address), RegistrationError> {
        let signer = self.signer_address().await?;

        let result =
            tokio::time::timeout(CHECK_TIMEOUT, self.registry.is_valid_signer(signer)).await;

        match result {
            Ok(Ok(valid)) => {
                if !valid {
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering};

    use base_proof_contracts::TEEProverRegistryClient;

    use super::*;
    use crate::test_utils::MockRegistry;

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
        let registry = base_proof_contracts::TEEProverRegistryContractClient::new(
            alloy_primitives::Address::ZERO,
            dummy_url,
        );
        RegistrationChecker::new(transport, registry)
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
}
