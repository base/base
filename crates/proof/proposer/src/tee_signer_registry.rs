//! Fresh registration snapshots for reactive TEE proof cleanup.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use async_trait::async_trait;
use base_proof_contracts::{AggregateVerifierContractClient, DisputeGameFactoryClient};

use crate::ProposerError;

/// Reads the registered signers used to verify new TEE proposals.
///
/// Destructive callers must fetch a fresh snapshot, rather than cache an absent signer.
/// Registration is distinct from the registry's current image-hash policy, which is
/// validated by the fork-specific verifier during submission.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait TeeSignerRegistry: Send + Sync {
    /// Returns a fresh registration snapshot from the currently active registry.
    async fn registered_signers(&self) -> Result<Vec<Address>, ProposerError>;
}

/// Discovers the active TEE registry through the factory's current game implementation.
pub struct OnchainTeeSignerRegistry {
    factory: Arc<dyn DisputeGameFactoryClient>,
    verifier: Arc<AggregateVerifierContractClient>,
    game_type: u32,
    read_timeout: Duration,
}

impl std::fmt::Debug for OnchainTeeSignerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnchainTeeSignerRegistry")
            .field("game_type", &self.game_type)
            .field("read_timeout", &self.read_timeout)
            .finish_non_exhaustive()
    }
}

impl OnchainTeeSignerRegistry {
    /// Creates a bounded registry reader that observes implementation and registry upgrades.
    pub const fn new(
        factory: Arc<dyn DisputeGameFactoryClient>,
        verifier: Arc<AggregateVerifierContractClient>,
        game_type: u32,
        read_timeout: Duration,
    ) -> Self {
        Self { factory, verifier, game_type, read_timeout }
    }
}

#[async_trait]
impl TeeSignerRegistry for OnchainTeeSignerRegistry {
    async fn registered_signers(&self) -> Result<Vec<Address>, ProposerError> {
        tokio::time::timeout(self.read_timeout, async {
            let implementation = self
                .factory
                .game_impls(self.game_type)
                .await
                .map_err(|error| ProposerError::Contract(error.to_string()))?;
            if implementation.is_zero() {
                return Err(ProposerError::Contract("game implementation address is zero".into()));
            }
            self.verifier
                .registered_tee_signers(implementation)
                .await
                .map_err(|error| ProposerError::Contract(error.to_string()))
        })
        .await
        .map_err(|_| ProposerError::Contract("TEE signer registry read timed out".into()))?
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

    use alloy_primitives::hex;
    use alloy_provider::RootProvider;
    use alloy_sol_types::SolValue;
    use base_proof_contracts::DisputeGameFactoryContractClient;
    use jsonrpsee::{RpcModule, server::ServerBuilder, types::ErrorObjectOwned};
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;

    use super::*;

    const FACTORY: Address = Address::repeat_byte(1);
    const IMPLEMENTATION: Address = Address::repeat_byte(2);
    const VERIFIER: Address = Address::repeat_byte(3);
    const REGISTRY: Address = Address::repeat_byte(4);

    fn registry_reader(endpoint: String, read_timeout: Duration) -> OnchainTeeSignerRegistry {
        let provider = RootProvider::new_http(endpoint.parse().unwrap());
        OnchainTeeSignerRegistry::new(
            Arc::new(DisputeGameFactoryContractClient::new(FACTORY, provider.clone())),
            Arc::new(AggregateVerifierContractClient::new(provider)),
            0,
            read_timeout,
        )
    }

    #[tokio::test]
    async fn registry_snapshot_follows_active_implementation_and_registry_upgrades() {
        let next_implementation = Address::repeat_byte(5);
        let next_verifier = Address::repeat_byte(6);
        let next_registry = Address::repeat_byte(7);
        let old_signer = Address::repeat_byte(0x11);
        let new_signer = Address::repeat_byte(0x22);
        let upgraded = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&upgraded);
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let mut module = RpcModule::new(());
        module
            .register_method("eth_call", move |params, _, _| {
                let params: Vec<Value> = params.parse()?;
                let address: Address = serde_json::from_value(params[0]["to"].clone()).unwrap();
                let encoded = if address == FACTORY {
                    let active = if observed.load(Ordering::SeqCst) {
                        next_implementation
                    } else {
                        IMPLEMENTATION
                    };
                    (active,).abi_encode()
                } else if address == IMPLEMENTATION {
                    (VERIFIER,).abi_encode()
                } else if address == next_implementation {
                    (next_verifier,).abi_encode()
                } else if address == VERIFIER {
                    (REGISTRY,).abi_encode()
                } else if address == next_verifier {
                    (next_registry,).abi_encode()
                } else if address == REGISTRY {
                    vec![old_signer].abi_encode()
                } else {
                    assert_eq!(address, next_registry);
                    vec![new_signer].abi_encode()
                };
                Ok::<_, ErrorObjectOwned>(format!("0x{}", hex::encode(encoded)))
            })
            .unwrap();
        let handle = server.start(module);
        let registry = registry_reader(endpoint, Duration::from_secs(2));

        assert_eq!(registry.registered_signers().await.unwrap(), vec![old_signer]);
        upgraded.store(true, Ordering::SeqCst);
        assert_eq!(registry.registered_signers().await.unwrap(), vec![new_signer]);

        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn registry_read_distinguishes_empty_snapshots_from_failed_or_missing_contracts() {
        const EMPTY: u8 = 0;
        const FAILED_READ: u8 = 1;
        const MISSING_IMPLEMENTATION: u8 = 2;
        const MISSING_REGISTRY: u8 = 3;

        let mode = Arc::new(AtomicU8::new(EMPTY));
        let observed = Arc::clone(&mode);
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let mut module = RpcModule::new(());
        module
            .register_method("eth_call", move |params, _, _| {
                let params: Vec<Value> = params.parse()?;
                let address: Address = serde_json::from_value(params[0]["to"].clone()).unwrap();
                let mode = observed.load(Ordering::SeqCst);
                let encoded = if address == FACTORY {
                    (if mode == MISSING_IMPLEMENTATION { Address::ZERO } else { IMPLEMENTATION },)
                        .abi_encode()
                } else if address == IMPLEMENTATION {
                    (VERIFIER,).abi_encode()
                } else if address == VERIFIER {
                    (if mode == MISSING_REGISTRY { Address::ZERO } else { REGISTRY },).abi_encode()
                } else {
                    assert_eq!(address, REGISTRY);
                    if mode == FAILED_READ {
                        return Err(ErrorObjectOwned::owned(
                            -32000,
                            "registry read failed",
                            None::<()>,
                        ));
                    }
                    Vec::<Address>::new().abi_encode()
                };
                Ok::<_, ErrorObjectOwned>(format!("0x{}", hex::encode(encoded)))
            })
            .unwrap();
        let handle = server.start(module);
        let registry = registry_reader(endpoint, Duration::from_secs(2));

        assert!(registry.registered_signers().await.unwrap().is_empty());
        for failure in [FAILED_READ, MISSING_IMPLEMENTATION, MISSING_REGISTRY] {
            mode.store(failure, Ordering::SeqCst);
            assert!(matches!(registry.registered_signers().await, Err(ProposerError::Contract(_))));
        }

        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn registry_snapshot_timeout_bounds_a_stalled_contract_read() {
        let read_started = CancellationToken::new();
        let release = CancellationToken::new();
        let started = read_started.clone();
        let unblock = release.clone();
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let mut module = RpcModule::new(());
        module
            .register_async_method("eth_call", move |params, _, _| {
                let started = started.clone();
                let unblock = unblock.clone();
                async move {
                    let params: Vec<Value> = params.parse()?;
                    let address: Address = serde_json::from_value(params[0]["to"].clone()).unwrap();
                    let encoded = if address == FACTORY {
                        (IMPLEMENTATION,).abi_encode()
                    } else if address == IMPLEMENTATION {
                        (VERIFIER,).abi_encode()
                    } else if address == VERIFIER {
                        (REGISTRY,).abi_encode()
                    } else {
                        assert_eq!(address, REGISTRY);
                        started.cancel();
                        unblock.cancelled().await;
                        Vec::<Address>::new().abi_encode()
                    };
                    Ok::<_, ErrorObjectOwned>(format!("0x{}", hex::encode(encoded)))
                }
            })
            .unwrap();
        let handle = server.start(module);
        let registry = registry_reader(endpoint, Duration::from_millis(500));

        let result =
            tokio::time::timeout(Duration::from_secs(2), registry.registered_signers()).await;
        release.cancel();
        handle.stop().unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle.stopped()).await.unwrap();

        assert!(read_started.is_cancelled(), "the test must reach the stalled registry read");
        assert!(matches!(
            result.expect("the reader's timeout must complete before the outer deadline"),
            Err(ProposerError::Contract(message)) if message.contains("timed out")
        ));
    }
}
