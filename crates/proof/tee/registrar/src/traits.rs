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
/// adapts a discovered endpoint [`Url`] to the shared
/// `base_proof_primitives::EnclaveApiClient` JSON-RPC surface. Test code can
/// substitute a mock to avoid real HTTP calls.
///
/// Implementations must return public keys and attestations in the same stable
/// signer order across calls for a given endpoint. The registrar pairs each
/// attestation response with the public-key response by index.
///
/// The `endpoint` parameter is a [`Url`] (e.g. `http://10.0.1.5:8000/`).
pub trait EnclaveEndpointClient: Send + Sync {
    /// Fetches the SEC1-encoded public key for each enclave signer at the given endpoint.
    fn signer_public_key<'a>(
        &'a self,
        endpoint: &'a Url,
    ) -> impl Future<Output = Result<Vec<Vec<u8>>>> + Send + 'a;

    /// Fetches the raw Nitro attestation document for each enclave signer at the given endpoint.
    ///
    /// The optional `nonces` vector must have one entry per signer in the same
    /// order returned by [`signer_public_key`](Self::signer_public_key).
    fn signer_attestation<'a>(
        &'a self,
        endpoint: &'a Url,
        nonces: Option<Vec<Vec<u8>>>,
    ) -> impl Future<Output = Result<Vec<Vec<u8>>>> + Send + 'a;
}
