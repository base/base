//! Registration-gated health check for the nitro prover.
//!
//! Delegates signer validity checks to [`RegistrationChecker`], which is shared
//! with the proving guard in `server.rs`.

use std::sync::Arc;

use alloy_primitives::Address;
use base_health::{HealthzApiServer, HealthzResponse};
use jsonrpsee::core::{RpcResult, async_trait};

use super::registration::RegistrationChecker;

/// Configuration for registration-gated health checks.
#[derive(Debug)]
pub struct RegistrationHealthConfig {
    /// `TEEProverRegistry` contract address on L1.
    pub registry_address: Address,
    /// L1 JSON-RPC endpoint URL.
    pub l1_rpc_url: String,
}

/// JSON-RPC handler for registration-gated health checks.
///
/// Uses the shared [`RegistrationChecker`] with a latching policy: once the
/// signer has been confirmed valid, health stays healthy forever (avoids ASG
/// replacement on transient L1 failures).
pub struct RegistrationHealthzRpc {
    version: &'static str,
    checker: Arc<RegistrationChecker>,
}

impl RegistrationHealthzRpc {
    /// Creates a new health check handler backed by the shared checker.
    pub const fn new(version: &'static str, checker: Arc<RegistrationChecker>) -> Self {
        Self { version, checker }
    }
}

impl std::fmt::Debug for RegistrationHealthzRpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrationHealthzRpc")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl HealthzApiServer for RegistrationHealthzRpc {
    async fn healthz(&self) -> RpcResult<HealthzResponse> {
        match self.checker.check_health().await {
            Ok(true) => Ok(HealthzResponse { version: self.version.to_string() }),
            Ok(false) => Err(jsonrpsee::types::ErrorObjectOwned::owned(
                -32000,
                "signer is not a valid signer in TEEProverRegistry",
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
    use std::sync::{Arc, atomic::Ordering};

    use base_proof_contracts::TEEProverRegistryClient;

    use super::*;
    use crate::{test_utils::MockRegistry, transport::NitroTransport};

    fn test_healthz_with_mock(
        registry: impl TEEProverRegistryClient + 'static,
    ) -> (Arc<RegistrationChecker>, RegistrationHealthzRpc) {
        let server = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let checker = Arc::new(RegistrationChecker::new(vec![transport], registry).unwrap());
        let rpc = RegistrationHealthzRpc::new("0.0.0", Arc::clone(&checker));
        (checker, rpc)
    }

    #[tokio::test]
    async fn healthz_returns_ok_when_valid() {
        let (_checker, rpc) = test_healthz_with_mock(MockRegistry::new(true));
        let result = HealthzApiServer::healthz(&rpc).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().version, "0.0.0");
    }

    #[tokio::test]
    async fn healthz_returns_error_when_not_valid() {
        let (_checker, rpc) = test_healthz_with_mock(MockRegistry::new(false));
        let result = HealthzApiServer::healthz(&rpc).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn healthz_latches_after_first_success() {
        let registry = MockRegistry::new(true);
        let (_checker, rpc) = test_healthz_with_mock(registry.clone());

        let result = HealthzApiServer::healthz(&rpc).await;
        assert!(result.is_ok());

        registry.valid.store(false, Ordering::Relaxed);
        registry.should_fail.store(true, Ordering::Relaxed);

        let result = HealthzApiServer::healthz(&rpc).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn healthz_errors_on_rpc_failure_before_latch() {
        let registry = MockRegistry::new(false);
        registry.should_fail.store(true, Ordering::Relaxed);
        let (_checker, rpc) = test_healthz_with_mock(registry);
        let result = HealthzApiServer::healthz(&rpc).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn healthz_rpc_call_count() {
        let registry = MockRegistry::new(true);
        let call_count = Arc::clone(&registry.call_count);
        let (_checker, rpc) = test_healthz_with_mock(registry);

        let _ = HealthzApiServer::healthz(&rpc).await;
        assert_eq!(call_count.load(Ordering::Relaxed), 1);

        let _ = HealthzApiServer::healthz(&rpc).await;
        assert_eq!(call_count.load(Ordering::Relaxed), 1);
    }
}
