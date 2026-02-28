//! RPC server implementation.
//!
//! Implements `EnclaveApiServer` for `Arc<Server>`.

use std::sync::Arc;

use alloy_primitives::Bytes;
use async_trait::async_trait;
use base_enclave::{ExecuteStatelessRequest, Proposal};
use jsonrpsee::types::ErrorObjectOwned;

use super::{api::EnclaveApiServer, types::AggregateRequest};
use crate::{Server, error::ServerError};

/// RPC server implementation wrapping the core `Server`.
#[derive(Debug, Clone)]
pub struct RpcServerImpl {
    server: Arc<Server>,
}

impl RpcServerImpl {
    /// Create a new RPC server implementation.
    pub const fn new(server: Arc<Server>) -> Self {
        Self { server }
    }
}

/// Convert a `ServerError` to a JSON-RPC error.
fn to_rpc_error(err: ServerError) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(-32000, err.to_string(), None::<()>)
}

#[async_trait]
impl EnclaveApiServer for RpcServerImpl {
    async fn signer_public_key(&self) -> Result<Bytes, ErrorObjectOwned> {
        Ok(Bytes::from(self.server.signer_public_key()))
    }

    async fn signer_attestation(&self) -> Result<Bytes, ErrorObjectOwned> {
        self.server.signer_attestation().map(Bytes::from).map_err(to_rpc_error)
    }

    async fn decryption_public_key(&self) -> Result<Bytes, ErrorObjectOwned> {
        self.server.decryption_public_key().map(Bytes::from).map_err(to_rpc_error)
    }

    async fn decryption_attestation(&self) -> Result<Bytes, ErrorObjectOwned> {
        self.server.decryption_attestation().map(Bytes::from).map_err(to_rpc_error)
    }

    async fn encrypted_signer_key(&self, attestation: Bytes) -> Result<Bytes, ErrorObjectOwned> {
        self.server.encrypted_signer_key(&attestation).map(Bytes::from).map_err(to_rpc_error)
    }

    async fn set_signer_key(&self, encrypted: Bytes) -> Result<(), ErrorObjectOwned> {
        self.server.set_signer_key(&encrypted).map_err(to_rpc_error)
    }

    async fn execute_stateless(
        &self,
        request: ExecuteStatelessRequest,
    ) -> Result<Proposal, ErrorObjectOwned> {
        self.server.execute_stateless(request).map_err(to_rpc_error)
    }

    async fn aggregate(&self, request: AggregateRequest) -> Result<Proposal, ErrorObjectOwned> {
        self.server
            .aggregate(
                request.config_hash,
                request.prev_output_root,
                request.prev_block_number,
                &request.proposals,
                request.proposer,
                request.tee_image_hash,
                &request.intermediate_roots,
            )
            .map_err(to_rpc_error)
    }
}
