//! RPC API trait definition using jsonrpsee proc macros.

use alloy_primitives::Bytes;
use base_enclave::{ExecuteStatelessRequest, Proposal};
use jsonrpsee::proc_macros::rpc;

use super::types::AggregateRequest;

/// Enclave RPC API trait.
///
/// This trait defines the JSON-RPC interface for the enclave server.
/// All methods are prefixed with `enclave_` in the RPC namespace.
#[rpc(server, client, namespace = "enclave")]
pub trait EnclaveApi {
    /// Get the signer's public key as a 65-byte uncompressed EC point.
    ///
    /// Returns the public key in the format used by Go's `crypto.FromECDSAPub()`.
    #[method(name = "signerPublicKey")]
    async fn signer_public_key(&self) -> Result<Bytes, jsonrpsee::types::ErrorObjectOwned>;

    /// Get an attestation document containing the signer's public key.
    ///
    /// Returns an error in local mode (NSM not available).
    #[method(name = "signerAttestation")]
    async fn signer_attestation(&self) -> Result<Bytes, jsonrpsee::types::ErrorObjectOwned>;

    /// Execute stateless block validation and create a signed proposal.
    ///
    /// Validates the block against the provided witness and returns a signed
    /// proposal containing the output root.
    #[method(name = "executeStateless")]
    async fn execute_stateless(
        &self,
        request: ExecuteStatelessRequest,
    ) -> Result<Proposal, jsonrpsee::types::ErrorObjectOwned>;

    /// Aggregate multiple proposals into a single proposal.
    ///
    /// Verifies the signature chain of all proposals and creates a new
    /// aggregated proposal spanning from the first to last block.
    #[method(name = "aggregate")]
    async fn aggregate(
        &self,
        request: AggregateRequest,
    ) -> Result<Proposal, jsonrpsee::types::ErrorObjectOwned>;
}
