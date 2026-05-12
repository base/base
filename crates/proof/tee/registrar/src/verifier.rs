//! `NitroEnclaveVerifier` client adapter for the registrar.
//!
//! Provides the durable on-chain revocation check (CHAIN-4194 / Immunefi
//! #75608) that the driver consults before submitting a registration: even
//! if the AWS CRL is silent, a previously-revoked intermediate must continue
//! to fail registration until an operator explicitly re-trusts it via
//! `unrevokeCert`.

use alloy_primitives::{Address, FixedBytes};
use async_trait::async_trait;
use base_proof_contracts::{NitroEnclaveVerifierClient as _, NitroEnclaveVerifierContractClient};
use url::Url;

use crate::{RegistrarError, Result};

/// Reads the durable revocation sentinel from the on-chain
/// `NitroEnclaveVerifier`.
#[async_trait]
pub trait NitroVerifierClient: Send + Sync {
    /// Returns the on-chain address of the verifier this client is bound to.
    /// Used by the driver as the destination for `revokeCert` transactions.
    fn address(&self) -> Address;

    /// Returns `true` if the given accumulated-path-digest hash is currently
    /// marked as revoked on-chain.
    async fn is_revoked(&self, cert_hash: FixedBytes<32>) -> Result<bool>;
}

/// Concrete implementation of [`NitroVerifierClient`] backed by the shared
/// [`NitroEnclaveVerifierContractClient`] from `base-proof-contracts`.
#[derive(Debug)]
pub struct NitroVerifierContractClient {
    inner: NitroEnclaveVerifierContractClient,
}

impl NitroVerifierContractClient {
    /// Creates a new client for the given verifier address and L1 RPC URL.
    pub fn new(address: Address, l1_rpc_url: Url) -> Self {
        Self { inner: NitroEnclaveVerifierContractClient::new(address, l1_rpc_url) }
    }
}

#[async_trait]
impl NitroVerifierClient for NitroVerifierContractClient {
    fn address(&self) -> Address {
        self.inner.address()
    }

    async fn is_revoked(&self, cert_hash: FixedBytes<32>) -> Result<bool> {
        self.inner.is_revoked(cert_hash).await.map_err(|e| RegistrarError::NitroVerifierCall {
            context: format!("revokedCerts({cert_hash:#x})"),
            source: Box::new(e),
        })
    }
}
