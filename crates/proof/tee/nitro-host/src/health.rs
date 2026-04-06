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
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use alloy_primitives::{Address, address};
    use base_proof_contracts::TEEProverRegistryClient;
    use jsonrpsee::core::async_trait;

    use super::*;
    use crate::transport::NitroTransport;

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

    fn test_healthz_with_mock(
        registry: impl TEEProverRegistryClient + 'static,
    ) -> (Arc<RegistrationChecker>, RegistrationHealthzRpc) {
        let server = Arc::new(base_proof_tee_nitro_enclave::Server::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let checker = Arc::new(RegistrationChecker::new(transport, registry));
        let rpc = RegistrationHealthzRpc::new("0.0.0", Arc::clone(&checker));
        (checker, rpc)
    }

    #[tokio::test]
    async fn healthz_returns_ok_when_valid() {
        let (checker, rpc) = test_healthz_with_mock(MockRegistry::new(true));
        checker.set_signer_for_test(TEST_SIGNER);
        let result = HealthzApiServer::healthz(&rpc).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().version, "0.0.0");
    }

    #[tokio::test]
    async fn healthz_returns_error_when_not_valid() {
        let (checker, rpc) = test_healthz_with_mock(MockRegistry::new(false));
        checker.set_signer_for_test(TEST_SIGNER);
        let result = HealthzApiServer::healthz(&rpc).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn healthz_latches_after_first_success() {
        let registry = MockRegistry::new(true);
        let (checker, rpc) = test_healthz_with_mock(registry.clone());
        checker.set_signer_for_test(TEST_SIGNER);

        let result = HealthzApiServer::healthz(&rpc).await;
        assert!(result.is_ok());

        registry.valid.store(false, Ordering::Relaxed);
        registry.should_fail.store(true, Ordering::Relaxed);
        checker.set_cache_for_test(None).await;

        let result = HealthzApiServer::healthz(&rpc).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn healthz_errors_on_rpc_failure_before_latch() {
        let registry = MockRegistry::new(false);
        registry.should_fail.store(true, Ordering::Relaxed);
        let (checker, rpc) = test_healthz_with_mock(registry);
        checker.set_signer_for_test(TEST_SIGNER);
        let result = HealthzApiServer::healthz(&rpc).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn healthz_rpc_call_count() {
        let registry = MockRegistry::new(true);
        let call_count = Arc::clone(&registry.call_count);
        let (checker, rpc) = test_healthz_with_mock(registry);
        checker.set_signer_for_test(TEST_SIGNER);

        let _ = HealthzApiServer::healthz(&rpc).await;
        assert_eq!(call_count.load(Ordering::Relaxed), 1);

        let _ = HealthzApiServer::healthz(&rpc).await;
        assert_eq!(call_count.load(Ordering::Relaxed), 1);
    }
}
