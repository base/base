use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, keccak256};
use base_health::{HealthzApiServer, HealthzResponse};
use base_proof_contracts::{TEEProverRegistryClient, TEEProverRegistryContractClient};
use jsonrpsee::core::{RpcResult, async_trait};
use tokio::sync::RwLock;
use tracing::warn;

use super::transport::NitroTransport;

const REGISTRATION_CACHE_TTL: Duration = Duration::from_secs(30);
const REGISTRATION_STALE_LIMIT: Duration = Duration::from_secs(300);

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
    cache: RwLock<Option<(bool, Instant)>>,
}

impl RegistrationHealthzRpc {
    pub(crate) fn new(
        version: &'static str,
        transport: Arc<NitroTransport>,
        registry: TEEProverRegistryContractClient,
    ) -> Self {
        Self { version, transport, registry, cache: RwLock::new(None) }
    }

    async fn check_registration(&self) -> Result<bool, String> {
        // Return cached result if fresh.
        {
            let cache = self.cache.read().await;
            if let Some((registered, checked_at)) = *cache
                && checked_at.elapsed() < REGISTRATION_CACHE_TTL {
                    return Ok(registered);
                }
        }

        let public_key = self
            .transport
            .signer_public_key()
            .await
            .map_err(|e| format!("failed to get signer public key: {e}"))?;

        let signer = derive_signer_address(&public_key)?;

        match self.registry.is_registered_signer(signer).await {
            Ok(registered) => {
                *self.cache.write().await = Some((registered, Instant::now()));
                if !registered {
                    warn!(signer = %signer, "signer is not registered in TEEProverRegistry");
                }
                Ok(registered)
            }
            Err(e) => {
                // On L1 RPC failure, return stale cached value if within the stale limit.
                let cache = self.cache.read().await;
                if let Some((registered, checked_at)) = *cache
                    && checked_at.elapsed() < REGISTRATION_STALE_LIMIT {
                        warn!(
                            error = %e,
                            signer = %signer,
                            stale_secs = checked_at.elapsed().as_secs(),
                            "L1 RPC failed, using stale cached registration status"
                        );
                        return Ok(registered);
                    }
                Err(format!("failed to check registration for {signer}: {e}"))
            }
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

fn derive_signer_address(public_key: &[u8]) -> Result<Address, String> {
    let key = k256::PublicKey::from_sec1_bytes(public_key)
        .map_err(|e| format!("invalid public key: {e}"))?;
    let uncompressed = k256::elliptic_curve::sec1::ToEncodedPoint::to_encoded_point(&key, false);
    let hash = keccak256(&uncompressed.as_bytes()[1..]);
    Ok(Address::from_slice(&hash[12..]))
}
