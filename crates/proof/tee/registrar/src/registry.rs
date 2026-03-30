//! Registry client adapter for the registrar.
//!
//! Wraps [`TEEProverRegistryContractClient`] from `base-proof-contracts` with
//! registrar-specific error types. The [`RegistryClient`] trait uses
//! [`RegistrarError`] to integrate with the registrar's error handling,
//! while the underlying contract bindings use [`ContractError`].

use alloy_primitives::Address;
use async_trait::async_trait;
use base_proof_contracts::{
    ContractError, TEEProverRegistryClient as _, TEEProverRegistryContractClient,
};
use url::Url;

use crate::{RegistrarError, Result};

/// Reads registration state from the on-chain `TEEProverRegistry`.
#[async_trait]
pub trait RegistryClient: Send + Sync {
    /// Returns `true` if `signer` is currently registered on-chain.
    async fn is_registered(&self, signer: Address) -> Result<bool>;

    /// Fetches the complete set of registered signer addresses in a single view call.
    ///
    /// The signer set is expected to be small (bounded by the prover ASG size, typically 4),
    /// so returning the full array in one call is appropriate. This assumption holds as long
    /// as the ASG is configured with a fixed, small instance count.
    async fn get_registered_signers(&self) -> Result<Vec<Address>>;
}

/// Concrete implementation of [`RegistryClient`] backed by the shared
/// [`TEEProverRegistryContractClient`] from `base-proof-contracts`.
#[derive(Debug)]
pub struct RegistryContractClient {
    inner: TEEProverRegistryContractClient,
}

impl RegistryContractClient {
    /// Creates a new client for the given registry address and L1 RPC URL.
    pub fn new(address: Address, l1_rpc_url: Url) -> Self {
        Self { inner: TEEProverRegistryContractClient::new(address, l1_rpc_url) }
    }
}

/// Converts a [`ContractError`] into a [`RegistrarError::RegistryCall`].
fn map_contract_error(e: ContractError) -> RegistrarError {
    RegistrarError::RegistryCall { context: e.to_string(), source: Box::new(e) }
}

#[async_trait]
impl RegistryClient for RegistryContractClient {
    async fn is_registered(&self, signer: Address) -> Result<bool> {
        self.inner.is_registered_signer(signer).await.map_err(map_contract_error)
    }

    async fn get_registered_signers(&self) -> Result<Vec<Address>> {
        self.inner.get_registered_signers().await.map_err(map_contract_error)
    }
}
