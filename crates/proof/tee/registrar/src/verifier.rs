//! `NitroEnclaveVerifier` client adapter for the registrar.
//!
//! Wraps [`NitroEnclaveVerifierContractClient`] from `base-proof-contracts`
//! with registrar-specific error types. The [`NitroVerifierClient`] trait uses
//! [`RegistrarError`] to integrate with the registrar's error handling; the
//! underlying contract bindings raise their own `ContractError`, which is
//! mapped inline to `RegistrarError::NitroVerifierCall` in `is_revoked`.
//!
//! Provides the durable on-chain revocation check that the driver consults
//! before submitting a registration. The check is the offchain counterpart to
//! the persistent `revokedCerts` sentinel introduced for CHAIN-4194 / Immunefi
//! #75608: even if the AWS CRL is silent, a previously-revoked intermediate
//! must continue to fail registration until an operator explicitly re-trusts
//! it via `unrevokeCert`.

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
    /// Used by the driver as the destination for `revokeCert` transactions
    /// submitted via the shared tx manager, eliminating the need to thread
    /// the address through a separate config field.
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
        // `context` is a structured call label (matching the
        // `NitroVerifierCall::context` docstring example), distinct from the
        // underlying error rendered via `#[source]`. Avoids the duplicate
        // message that `e.to_string()` would produce when the error chain
        // is walked.
        self.inner.is_revoked(cert_hash).await.map_err(|e| RegistrarError::NitroVerifierCall {
            context: format!("revokedCerts({cert_hash:#x})"),
            source: Box::new(e),
        })
    }
}
