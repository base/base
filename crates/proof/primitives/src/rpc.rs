// This module requires at least one of the RPC features to compile correctly.
// The `lib.rs` cfg gate normally ensures this, but we add an explicit guard for
// safety in case the module is ever included directly.
#[cfg(not(any(feature = "rpc-server", feature = "rpc-client")))]
compile_error!("this module requires the `rpc-server` or `rpc-client` feature");

// The jsonrpsee `rpc` proc macro rewrites the trait depending on which variant
// is selected (`server`, `client`, or both).  We must pick exactly the right
// combination for the enabled feature set so the generated code only references
// dependencies that are actually compiled in.
//
// `RpcResult` is only used by the *server* side of the generated code; when
// only `rpc-client` is active the macro rewrites signatures to use its own
// client return type, so the import would be dead code.
#[cfg(feature = "rpc-server")]
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;

use crate::{ProofRequest, ProofResult};

#[cfg_attr(
    all(feature = "rpc-server", feature = "rpc-client"),
    rpc(server, client, namespace = "prover")
)]
#[cfg_attr(
    all(feature = "rpc-server", not(feature = "rpc-client")),
    rpc(server, namespace = "prover")
)]
#[cfg_attr(
    all(feature = "rpc-client", not(feature = "rpc-server")),
    rpc(client, namespace = "prover")
)]
/// JSON-RPC interface shared by all proof backends.
pub trait ProverApi {
    /// Run the proof pipeline for a single request.
    #[method(name = "prove")]
    async fn prove(&self, request: ProofRequest) -> RpcResult<ProofResult>;
}

/// Trait-object friendly proof client used by proposer pipelines.
#[cfg(feature = "rpc-client")]
#[async_trait::async_trait]
pub trait ProverClient: core::fmt::Debug + Send + Sync {
    /// Run the proof pipeline for a single request.
    async fn prove(
        &self,
        request: ProofRequest,
    ) -> Result<ProofResult, Box<dyn std::error::Error + Send + Sync>>;
}

#[cfg(feature = "rpc-client")]
#[async_trait::async_trait]
impl<T> ProverClient for T
where
    T: ProverApiClient + core::fmt::Debug + Send + Sync,
{
    async fn prove(
        &self,
        request: ProofRequest,
    ) -> Result<ProofResult, Box<dyn std::error::Error + Send + Sync>> {
        ProverApiClient::prove(self, request)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }
}

/// JSON-RPC interface for querying enclave signer information.
#[cfg_attr(
    all(feature = "rpc-server", feature = "rpc-client"),
    rpc(server, client, namespace = "enclave")
)]
#[cfg_attr(
    all(feature = "rpc-server", not(feature = "rpc-client")),
    rpc(server, namespace = "enclave")
)]
#[cfg_attr(
    all(feature = "rpc-client", not(feature = "rpc-server")),
    rpc(client, namespace = "enclave")
)]
/// Exposed by the host-side prover server; the registrar calls these endpoints
/// to obtain the signer public key and attestation for onchain registration.
pub trait EnclaveApi {
    /// Return the attestation platform kind exposed by this prover.
    #[method(name = "attestationKind")]
    async fn attestation_kind(&self) -> RpcResult<String> {
        Ok("nitro".to_owned())
    }

    /// Return the 65-byte uncompressed ECDSA public key for each enclave signer.
    #[method(name = "signerPublicKey")]
    async fn signer_public_key(&self) -> RpcResult<Vec<Vec<u8>>>;

    /// Return the platform-specific attestation bytes for each enclave signer.
    ///
    /// Optional `user_data` and per-signer `nonces` bind attestations to a specific request.
    /// When supplied, `nonces` must have one entry per signer in signer order.
    #[method(name = "signerAttestation")]
    async fn signer_attestation(
        &self,
        user_data: Option<Vec<u8>>,
        nonces: Option<Vec<Vec<u8>>>,
    ) -> RpcResult<Vec<Vec<u8>>>;
}
