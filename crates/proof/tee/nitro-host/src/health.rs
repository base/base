//! Registration-gated health check for the nitro prover.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::Address;
use alloy_signer::utils::public_key_to_address;
use base_health::{HealthzApiServer, HealthzResponse};
use base_proof_contracts::{TEEProverRegistryClient, TEEProverRegistryContractClient};
use jsonrpsee::core::{RpcResult, async_trait};
use k256::ecdsa::VerifyingKey;
use tokio::sync::{OnceCell, RwLock};
use tracing::warn;

use super::transport::NitroTransport;

const REGISTRATION_CACHE_TTL: Duration = Duration::from_secs(30);
const REGISTRATION_STALE_LIMIT: Duration = Duration::from_secs(300);
const REGISTRATION_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Configuration for registration-gated health checks.
#[derive(Debug)]
pub struct RegistrationHealthConfig {
    /// `TEEProverRegistry` contract address on L1.
    pub registry_address: Address,
    /// L1 JSON-RPC endpoint URL.
    pub l1_rpc_url: String,
}

pub(crate) struct RegistrationHealthzRpc {
    version: &'static str,
    transport: Arc<NitroTransport>,
    registry: TEEProverRegistryContractClient,
    signer: OnceCell<Address>,
    cache: RwLock<Option<(bool, Instant)>>,
}

impl RegistrationHealthzRpc {
    pub(crate) fn new(
        version: &'static str,
        transport: Arc<NitroTransport>,
        registry: TEEProverRegistryContractClient,
    ) -> Self {
        Self { version, transport, registry, signer: OnceCell::new(), cache: RwLock::new(None) }
    }

    async fn signer_address(&self) -> Result<Address, String> {
        self.signer
            .get_or_try_init(|| async {
                let public_key = self
                    .transport
                    .signer_public_key()
                    .await
                    .map_err(|e| format!("failed to get signer public key: {e}"))?;
                let verifying_key = VerifyingKey::from_sec1_bytes(&public_key)
                    .map_err(|e| format!("invalid public key: {e}"))?;
                Ok(public_key_to_address(&verifying_key))
            })
            .await
            .copied()
    }

    async fn use_stale_cache_or_fail(&self, signer: Address, error: &str) -> Result<bool, String> {
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
        Err(format!("failed to check registration for {signer}: {error}"))
    }

    async fn check_registration(&self) -> Result<bool, String> {
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
            Ok(Err(e)) => self.use_stale_cache_or_fail(signer, &e.to_string()).await,
            Err(_) => self.use_stale_cache_or_fail(signer, "L1 RPC request timed out").await,
        }
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
            Err(msg) => Err(jsonrpsee::types::ErrorObjectOwned::owned(-32000, msg, None::<()>)),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    fn test_rpc() -> RegistrationHealthzRpc {
        let server = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let dummy_url = url::Url::parse("http://localhost:1").unwrap();
        let registry = TEEProverRegistryContractClient::new(Address::ZERO, dummy_url);
        RegistrationHealthzRpc::new("0.0.0", transport, registry)
    }

    const TEST_SIGNER: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    #[tokio::test]
    async fn stale_cache_returns_cached_value_on_error() {
        let rpc = test_rpc();
        *rpc.cache.write().await = Some((true, Instant::now()));
        let result = rpc.use_stale_cache_or_fail(TEST_SIGNER, "rpc down").await;
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn stale_cache_fails_when_expired() {
        let rpc = test_rpc();
        let expired = Instant::now() - REGISTRATION_STALE_LIMIT - Duration::from_secs(1);
        *rpc.cache.write().await = Some((true, expired));
        let result = rpc.use_stale_cache_or_fail(TEST_SIGNER, "rpc down").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn stale_cache_fails_when_empty() {
        let rpc = test_rpc();
        let result = rpc.use_stale_cache_or_fail(TEST_SIGNER, "rpc down").await;
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
}
