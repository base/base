use jsonrpsee::{core::RpcResult, proc_macros::rpc};

use crate::{ProofRequest, ProofResult};

/// JSON-RPC interface shared by all proof backends.
#[cfg_attr(not(feature = "rpc-client"), rpc(server, namespace = "prover"))]
#[cfg_attr(feature = "rpc-client", rpc(server, client, namespace = "prover"))]
pub trait ProverApi {
    /// Run the proof pipeline for a single request.
    #[method(name = "prove")]
    async fn prove(&self, request: ProofRequest) -> RpcResult<ProofResult>;
}

/// JSON-RPC interface for querying enclave signer information.
///
/// Exposed by the host-side prover server; the registrar calls these endpoints
/// to obtain the signer public key and attestation for on-chain registration.
#[cfg_attr(not(feature = "rpc-client"), rpc(server, namespace = "enclave"))]
#[cfg_attr(feature = "rpc-client", rpc(server, client, namespace = "enclave"))]
pub trait EnclaveApi {
    /// Return the 65-byte uncompressed ECDSA public key for the enclave signer.
    #[method(name = "signerPublicKey")]
    async fn signer_public_key(&self) -> RpcResult<Vec<u8>>;

    /// Return the raw Nitro attestation document (`COSE_Sign1` bytes) for the enclave signer.
    #[method(name = "signerAttestation")]
    async fn signer_attestation(&self) -> RpcResult<Vec<u8>>;
}
