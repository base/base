//! Shared signer-registration checker backed by the on-chain `TEEProverRegistry`.
//!
//! Used by both the health endpoint (fail-open with stale cache) and the
//! proving guard (fail-closed) to avoid duplicating L1 contract queries.

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
pub(crate) const STALE_LIMIT: Duration = Duration::from_secs(300);

/// Structured error type for signer-registration checks.
#[derive(Debug, Error)]
pub enum RegistrationError {
    /// Failed to retrieve the signer public key from the enclave.
    #[error("failed to get signer public key: {0}")]
    SignerKey(String),
    /// The public key bytes are not a valid secp256k1 point.
    #[error("invalid public key: {0}")]
    InvalidPublicKey(String),
    /// The L1 RPC call to check registration status failed.
    #[error("L1 RPC call failed for signer {signer}: {reason}")]
    L1Rpc {
        /// The signer address that was being checked.
        signer: Address,
        /// The underlying RPC error message.
        reason: String,
    },
    /// The L1 RPC request timed out.
    #[error("L1 RPC request timed out for signer {signer}")]
    Timeout {
        /// The signer address that was being checked.
        signer: Address,
    },
    /// The signer is registered but its image hash does not match, or it is
    /// not registered at all.
    #[error("signer {signer} is not a valid signer in TEEProverRegistry")]
    NotValid {
        /// The signer address that failed validation.
        signer: Address,
    },
    /// Registration check failed and the stale cache has expired.
    #[error("registration check failed for {signer}: stale cache expired after {stale_secs}s")]
    StaleExpired {
        /// The signer address that was being checked.
        signer: Address,
        /// How many seconds the stale cache entry had been held.
        stale_secs: u64,
    },
}

/// Checks whether the enclave signer is a **valid** signer in the on-chain
/// `TEEProverRegistry` (registered AND matching the current image hash).
///
/// Results are cached for [`CACHE_TTL`] to avoid hitting L1 on every request.
pub struct RegistrationChecker {
    transport: Arc<NitroTransport>,
    registry: Box<dyn TEEProverRegistryClient>,
    signer: OnceCell<Address>,
    cache: RwLock<Option<(bool, Instant)>>,
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
        }
    }

    async fn signer_address(&self) -> Result<Address, RegistrationError> {
        self.signer
            .get_or_try_init(|| async {
                let public_key = self
                    .transport
                    .signer_public_key()
                    .await
                    .map_err(|e| RegistrationError::SignerKey(e.to_string()))?;
                let verifying_key = VerifyingKey::from_sec1_bytes(&public_key)
                    .map_err(|e| RegistrationError::InvalidPublicKey(e.to_string()))?;
                Ok(public_key_to_address(&verifying_key))
            })
            .await
            .copied()
    }

    async fn fetch_validity(&self) -> Result<bool, RegistrationError> {
        {
            let cache = self.cache.read().await;
            if let Some((valid, checked_at)) = *cache
                && checked_at.elapsed() < CACHE_TTL
            {
                return Ok(valid);
            }
        }

        let signer = self.signer_address().await?;

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
                Ok(valid)
            }
            Ok(Err(e)) => Err(RegistrationError::L1Rpc { signer, reason: e.to_string() }),
            Err(_) => Err(RegistrationError::Timeout { signer }),
        }
    }

    /// Returns the cached validity, falling back to stale cache within
    /// [`STALE_LIMIT`] when L1 is unreachable.  Used by the health endpoint
    /// (fail-open).
    pub async fn is_valid_signer_or_stale(&self) -> Result<bool, RegistrationError> {
        match self.fetch_validity().await {
            Ok(valid) => Ok(valid),
            Err(e @ (RegistrationError::SignerKey(_) | RegistrationError::InvalidPublicKey(_))) => {
                Err(e)
            }
            Err(RegistrationError::L1Rpc { signer, reason }) => {
                self.use_stale_cache_or_fail(signer, RegistrationError::L1Rpc { signer, reason })
                    .await
            }
            Err(RegistrationError::Timeout { signer }) => {
                self.use_stale_cache_or_fail(signer, RegistrationError::Timeout { signer }).await
            }
            Err(
                e @ (RegistrationError::NotValid { .. } | RegistrationError::StaleExpired { .. }),
            ) => Err(e),
        }
    }

    async fn use_stale_cache_or_fail(
        &self,
        signer: Address,
        rpc_err: RegistrationError,
    ) -> Result<bool, RegistrationError> {
        let cache = self.cache.read().await;
        if let Some((valid, checked_at)) = *cache {
            let elapsed = checked_at.elapsed();
            if elapsed < STALE_LIMIT {
                warn!(
                    error = %rpc_err,
                    signer = %signer,
                    stale_secs = elapsed.as_secs(),
                    "L1 RPC failed, using stale cached registration status"
                );
                return Ok(valid);
            }
        }
        let stale_secs = cache.map(|(_, checked_at)| checked_at.elapsed().as_secs()).unwrap_or(0);
        warn!(
            error = %rpc_err,
            signer = %signer,
            stale_secs,
            "stale cache expired, cannot verify signer"
        );
        Err(RegistrationError::StaleExpired { signer, stale_secs })
    }

    /// Fails the request unless the signer is currently valid.
    ///
    /// Fail-closed: does **not** fall back to stale cache.  If L1 is
    /// unreachable the proof request is rejected.
    pub async fn require_valid_signer(&self) -> Result<(), RegistrationError> {
        match self.fetch_validity().await {
            Ok(true) => Ok(()),
            Ok(false) => {
                let signer = self.signer_address().await?;
                Err(RegistrationError::NotValid { signer })
            }
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
    use std::time::Instant;

    use alloy_primitives::address;

    use super::*;

    fn test_checker() -> RegistrationChecker {
        let server = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let dummy_url = url::Url::parse("http://localhost:1").unwrap();
        let registry =
            base_proof_contracts::TEEProverRegistryContractClient::new(Address::ZERO, dummy_url);
        RegistrationChecker::new(transport, registry)
    }

    const TEST_SIGNER: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    #[tokio::test]
    async fn stale_cache_returns_cached_value_on_rpc_error() {
        let checker = test_checker();
        checker.signer.set(TEST_SIGNER).unwrap();
        let just_expired = Instant::now() - CACHE_TTL - Duration::from_secs(1);
        *checker.cache.write().await = Some((true, just_expired));
        let result = checker.is_valid_signer_or_stale().await;
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn stale_cache_fails_when_expired() {
        let checker = test_checker();
        checker.signer.set(TEST_SIGNER).unwrap();
        let expired = Instant::now() - STALE_LIMIT - Duration::from_secs(1);
        *checker.cache.write().await = Some((true, expired));
        let result = checker.is_valid_signer_or_stale().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn stale_cache_fails_when_empty() {
        let checker = test_checker();
        checker.signer.set(TEST_SIGNER).unwrap();
        let result = checker.is_valid_signer_or_stale().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cache_hit_within_ttl_returns_valid() {
        let checker = test_checker();
        checker.signer.set(TEST_SIGNER).unwrap();
        *checker.cache.write().await = Some((true, Instant::now()));
        let result = checker.is_valid_signer_or_stale().await;
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn cache_hit_returns_false_when_not_valid() {
        let checker = test_checker();
        checker.signer.set(TEST_SIGNER).unwrap();
        *checker.cache.write().await = Some((false, Instant::now()));
        let result = checker.is_valid_signer_or_stale().await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn require_valid_signer_ok_when_cached_valid() {
        let checker = test_checker();
        checker.signer.set(TEST_SIGNER).unwrap();
        *checker.cache.write().await = Some((true, Instant::now()));
        assert!(checker.require_valid_signer().await.is_ok());
    }

    #[tokio::test]
    async fn require_valid_signer_rejects_when_cached_invalid() {
        let checker = test_checker();
        checker.signer.set(TEST_SIGNER).unwrap();
        *checker.cache.write().await = Some((false, Instant::now()));
        let err = checker.require_valid_signer().await.unwrap_err();
        assert!(matches!(err, RegistrationError::NotValid { .. }));
    }

    #[tokio::test]
    async fn require_valid_signer_rejects_on_rpc_error() {
        let checker = test_checker();
        checker.signer.set(TEST_SIGNER).unwrap();
        let err = checker.require_valid_signer().await.unwrap_err();
        assert!(matches!(err, RegistrationError::L1Rpc { .. } | RegistrationError::Timeout { .. }));
    }

    #[tokio::test]
    async fn require_valid_signer_rejects_on_stale_cache() {
        let checker = test_checker();
        checker.signer.set(TEST_SIGNER).unwrap();
        let expired = Instant::now() - CACHE_TTL - Duration::from_secs(1);
        *checker.cache.write().await = Some((true, expired));
        let err = checker.require_valid_signer().await.unwrap_err();
        assert!(matches!(err, RegistrationError::L1Rpc { .. } | RegistrationError::Timeout { .. }));
    }
}
