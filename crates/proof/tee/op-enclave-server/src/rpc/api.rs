//! RPC API trait definition using jsonrpsee proc macros.
//!
//! This defines the `EnclaveApi` trait with the 8 RPC methods matching
//! the Go `RPC` interface in `enclave/rpc.go`.

use alloy_primitives::Bytes;
use jsonrpsee::proc_macros::rpc;

use op_enclave_core::Proposal;

use super::types::{AggregateRequest, ExecuteStatelessRequest};

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

    /// Get the decryption public key in PKIX/DER format.
    ///
    /// This matches Go's `x509.MarshalPKIXPublicKey()` format.
    #[method(name = "decryptionPublicKey")]
    async fn decryption_public_key(&self) -> Result<Bytes, jsonrpsee::types::ErrorObjectOwned>;

    /// Get an attestation document containing the decryption public key.
    ///
    /// Returns an error in local mode (NSM not available).
    #[method(name = "decryptionAttestation")]
    async fn decryption_attestation(&self) -> Result<Bytes, jsonrpsee::types::ErrorObjectOwned>;

    /// Encrypt the signer key for a remote enclave.
    ///
    /// Verifies the provided attestation document, checks that PCR0 matches,
    /// and encrypts the signer key using the public key from the attestation.
    #[method(name = "encryptedSignerKey")]
    async fn encrypted_signer_key(
        &self,
        attestation: Bytes,
    ) -> Result<Bytes, jsonrpsee::types::ErrorObjectOwned>;

    /// Set the signer key from an encrypted key.
    ///
    /// Decrypts the provided ciphertext using the decryption key
    /// and updates the signer key.
    #[method(name = "setSignerKey")]
    async fn set_signer_key(
        &self,
        encrypted: Bytes,
    ) -> Result<(), jsonrpsee::types::ErrorObjectOwned>;

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
