//! Registration-gated health check for the nitro prover.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::Address;
use alloy_signer::utils::public_key_to_address;
use base_health::{HealthzApiServer, HealthzResponse};
use base_proof_contracts::TEEProverRegistryClient;
use jsonrpsee::core::{RpcResult, async_trait};
use k256::ecdsa::VerifyingKey;
use tokio::sync::{OnceCell, RwLock};
use tracing::warn;

use super::transport::NitroTransport;

const REGISTRATION_CACHE_TTL: Duration = Duration::from_secs(30);
const REGISTRATION_STALE_LIMIT: Duration = Duration::from_secs(300);
const REGISTRATION_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors from the registration health check.
#[derive(Debug, thiserror::Error)]
pub enum RegistrationHealthError {
    /// Failed to retrieve the signer public key from the enclave.
    #[error("failed to get signer public key: {0}")]
    SignerKey(eyre::Report),
    /// The public key bytes are not a valid secp256k1 point.
    #[error("invalid public key: {0}")]
    InvalidPublicKey(String),
    /// The L1 RPC call to check registration status failed.
    #[error("L1 RPC call failed: {0}")]
    L1Rpc(base_proof_contracts::ContractError),
    /// The L1 RPC request timed out.
    #[error("L1 RPC request timed out")]
    Timeout,
    /// Registration check failed and stale cache has expired.
    #[error("registration check failed for {signer}: {reason}")]
    StaleExpired {
        /// The signer address that was being checked.
        signer: Address,
        /// The underlying reason the registration check failed.
        reason: String,
    },
}

/// Configuration for registration-gated health checks.
#[derive(Debug)]
pub struct RegistrationHealthConfig {
    /// `TEEProverRegistry` contract address on L1.
    pub registry_address: Address,
    /// L1 JSON-RPC endpoint URL.
    pub l1_rpc_url: String,
}

/// JSON-RPC handler for registration-gated health checks.
pub struct RegistrationHealthzRpc {
    version: &'static str,
    transport: Arc<NitroTransport>,
    registry: Box<dyn TEEProverRegistryClient>,
    signer: OnceCell<Address>,
    cache: RwLock<Option<(bool, Instant)>>,
}

impl RegistrationHealthzRpc {
    /// Creates a new health check handler.
    pub fn new(
        version: &'static str,
        transport: Arc<NitroTransport>,
        registry: impl TEEProverRegistryClient + 'static,
    ) -> Self {
        Self {
            version,
            transport,
            registry: Box::new(registry),
            signer: OnceCell::new(),
            cache: RwLock::new(None),
        }
    }

    async fn signer_address(&self) -> Result<Address, RegistrationHealthError> {
        self.signer
            .get_or_try_init(|| async {
                let public_key = self
                    .transport
                    .signer_public_key()
                    .await
                    .map_err(|e| RegistrationHealthError::SignerKey(e.into()))?;
                let verifying_key = VerifyingKey::from_sec1_bytes(&public_key)
                    .map_err(|e| RegistrationHealthError::InvalidPublicKey(e.to_string()))?;
                Ok(public_key_to_address(&verifying_key))
            })
            .await
            .copied()
    }

    async fn use_stale_cache_or_fail(
        &self,
        signer: Address,
        error: &(dyn std::fmt::Display + Sync),
    ) -> Result<bool, RegistrationHealthError> {
        let cache = self.cache.read().await;
        if let Some((registered, checked_at)) = *cache {
            let elapsed = checked_at.elapsed();
            if elapsed < REGISTRATION_STALE_LIMIT {
                warn!(
                    error = %error,
                    signer = %signer,
                    stale_secs = elapsed.as_secs(),
                    "L1 RPC failed, using stale cached registration status"
                );
                return Ok(registered);
            }
        }
        Err(RegistrationHealthError::StaleExpired { signer, reason: error.to_string() })
    }

    async fn check_registration(&self) -> Result<bool, RegistrationHealthError> {
        {
            let cache = self.cache.read().await;
            if let Some((registered, checked_at)) = *cache
                && checked_at.elapsed() < REGISTRATION_CACHE_TTL
            {
                return Ok(registered);
            }
        }

        let signer = self.signer_address().await?;

        let result = tokio::time::timeout(
            REGISTRATION_CHECK_TIMEOUT,
            self.registry.is_registered_signer(signer),
        )
        .await;

        match result {
            Ok(Ok(registered)) => {
                let mut cache = self.cache.write().await;
                let was_registered = cache.map(|(r, _)| r);
                *cache = Some((registered, Instant::now()));
                if !registered && was_registered != Some(false) {
                    warn!(signer = %signer, "signer is not registered in TEEProverRegistry");
                }
                Ok(registered)
            }
            Ok(Err(e)) => {
                let error = RegistrationHealthError::L1Rpc(e);
                self.use_stale_cache_or_fail(signer, &error).await
            }
            Err(_) => self.use_stale_cache_or_fail(signer, &RegistrationHealthError::Timeout).await,
        }
    }
}

impl std::fmt::Debug for RegistrationHealthzRpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrationHealthzRpc")
            .field("version", &self.version)
            .field("registry", &"dyn TEEProverRegistryClient")
            .field("signer", &self.signer.get())
            .finish()
    }
}

#[async_trait]
impl HealthzApiServer for RegistrationHealthzRpc {
    async fn healthz(&self) -> RpcResult<HealthzResponse> {
        match self.check_registration().await {
            Ok(true) => Ok(HealthzResponse { version: self.version.to_string() }),
            Ok(false) => Err(jsonrpsee::types::ErrorObjectOwned::owned(
                -32000,
                "signer not registered in TEEProverRegistry",
                None::<()>,
            )),
            Err(e) => {
                Err(jsonrpsee::types::ErrorObjectOwned::owned(-32000, e.to_string(), None::<()>))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use alloy_primitives::address;
    use base_proof_contracts::TEEProverRegistryContractClient;

    use super::*;

    #[derive(Clone)]
    struct MockRegistry {
        registered: Arc<AtomicBool>,
        call_count: Arc<AtomicUsize>,
        should_fail: Arc<AtomicBool>,
    }

    impl MockRegistry {
        fn new(registered: bool) -> Self {
            Self {
                registered: Arc::new(AtomicBool::new(registered)),
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
            unimplemented!("not used in health checks")
        }

        async fn is_registered_signer(
            &self,
            _signer: Address,
        ) -> Result<bool, base_proof_contracts::ContractError> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            if self.should_fail.load(Ordering::Relaxed) {
                return Err(base_proof_contracts::ContractError::Validation(
                    "mock RPC failure".into(),
                ));
            }
            Ok(self.registered.load(Ordering::Relaxed))
        }

        async fn get_registered_signers(
            &self,
        ) -> Result<Vec<Address>, base_proof_contracts::ContractError> {
            unimplemented!("not used in health checks")
        }
    }

    fn test_rpc_with_mock(
        registry: impl TEEProverRegistryClient + 'static,
    ) -> RegistrationHealthzRpc {
        let server = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        RegistrationHealthzRpc::new("0.0.0", transport, registry)
    }

    fn test_rpc() -> RegistrationHealthzRpc {
        let dummy_url = url::Url::parse("http://localhost:1").unwrap();
        let registry = TEEProverRegistryContractClient::new(Address::ZERO, dummy_url);
        test_rpc_with_mock(registry)
    }

    const TEST_SIGNER: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    #[tokio::test]
    async fn stale_cache_returns_cached_value_on_error() {
        let rpc = test_rpc();
        *rpc.cache.write().await = Some((true, Instant::now()));
        let result = rpc.use_stale_cache_or_fail(TEST_SIGNER, &"rpc down").await;
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn stale_cache_fails_when_expired() {
        let rpc = test_rpc();
        let expired = Instant::now() - REGISTRATION_STALE_LIMIT - Duration::from_secs(1);
        *rpc.cache.write().await = Some((true, expired));
        let result = rpc.use_stale_cache_or_fail(TEST_SIGNER, &"rpc down").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn stale_cache_fails_when_empty() {
        let rpc = test_rpc();
        let result = rpc.use_stale_cache_or_fail(TEST_SIGNER, &"rpc down").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cache_hit_within_ttl() {
        let rpc = test_rpc();
        rpc.signer.set(TEST_SIGNER).unwrap();
        *rpc.cache.write().await = Some((true, Instant::now()));
        let result = rpc.check_registration().await;
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn cache_hit_returns_false_when_not_registered() {
        let rpc = test_rpc();
        rpc.signer.set(TEST_SIGNER).unwrap();
        *rpc.cache.write().await = Some((false, Instant::now()));
        let result = rpc.check_registration().await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn check_registration_cache_miss_calls_rpc() {
        let registry = MockRegistry::new(true);
        let call_count = Arc::clone(&registry.call_count);
        let rpc = test_rpc_with_mock(registry);
        rpc.signer.set(TEST_SIGNER).unwrap();

        let result = rpc.check_registration().await;
        assert!(result.unwrap());
        assert_eq!(call_count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn check_registration_populates_cache_after_rpc() {
        let rpc = test_rpc_with_mock(MockRegistry::new(true));
        rpc.signer.set(TEST_SIGNER).unwrap();

        let result = rpc.check_registration().await;
        assert!(result.unwrap());

        let result = rpc.check_registration().await;
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn check_registration_rpc_failure_uses_stale_cache() {
        let failing = MockRegistry::new(false);
        failing.should_fail.store(true, Ordering::Relaxed);
        let rpc = test_rpc_with_mock(failing);
        rpc.signer.set(TEST_SIGNER).unwrap();

        let stale_time = Instant::now() - REGISTRATION_CACHE_TTL - Duration::from_secs(1);
        *rpc.cache.write().await = Some((true, stale_time));

        let result = rpc.check_registration().await;
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn check_registration_rpc_failure_no_cache_returns_error() {
        let failing = MockRegistry::new(false);
        failing.should_fail.store(true, Ordering::Relaxed);
        let rpc = test_rpc_with_mock(failing);
        rpc.signer.set(TEST_SIGNER).unwrap();

        let result = rpc.check_registration().await;
        assert!(result.is_err());
    }
}
