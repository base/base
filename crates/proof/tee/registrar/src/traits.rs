//! Abstraction traits for the registration driver.

use std::future::Future;

use url::Url;

use crate::{ProverInstance, Result};

/// Discovers active prover instances from the infrastructure layer.
///
/// The primary implementation is
/// [`AwsTargetGroupDiscovery`](crate::AwsTargetGroupDiscovery), which queries
/// an ALB target group via the AWS SDK. Other implementations (e.g., a static
/// list for local testing) can be substituted.
pub trait InstanceDiscovery: Send + Sync {
    /// Return the current set of prover instances with their health status.
    fn discover_instances(&self) -> impl Future<Output = Result<Vec<ProverInstance>>> + Send + '_;
}

/// Fetches signer identity data from a prover instance endpoint.
///
/// The primary implementation is [`ProverClient`](crate::ProverClient), which
/// makes JSON-RPC calls to the prover's `enclave_signerPublicKey` and
/// `enclave_signerAttestation` endpoints. Test code can substitute a mock
/// to avoid real HTTP calls.
///
/// The `endpoint` parameter is a [`Url`] (e.g. `http://10.0.1.5:8000/`).
pub trait SignerClient: Send + Sync {
    /// Fetches the SEC1-encoded public key for each enclave signer at the given endpoint.
    fn signer_public_key<'a>(
        &'a self,
        endpoint: &'a Url,
    ) -> impl Future<Output = Result<Vec<Vec<u8>>>> + Send + 'a;

    /// Fetches the raw Nitro attestation document for each enclave signer at the given endpoint.
    ///
    /// Optional `user_data` and `nonce` bind the attestation to a specific
    /// request (e.g. a random nonce for replay protection).
    fn signer_attestation<'a>(
        &'a self,
        endpoint: &'a Url,
        user_data: Option<Vec<u8>>,
        nonce: Option<Vec<u8>>,
    ) -> impl Future<Output = Result<Vec<Vec<u8>>>> + Send + 'a;
}
