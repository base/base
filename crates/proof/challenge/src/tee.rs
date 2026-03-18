//! TEE proof provider abstraction for TEE-first proof sourcing.

use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::BlockNumberOrTag;
use async_trait::async_trait;
use base_enclave_client::EnclaveClient;
use base_proof_primitives::{ProofRequest, ProofResult};

/// Trait for fetching the L1 finalized head hash.
#[async_trait]
pub trait L1HeadProvider: Send + Sync + std::fmt::Debug {
    /// Returns the hash of the current L1 finalized block.
    async fn finalized_head_hash(&self) -> eyre::Result<B256>;
}

/// [`L1HeadProvider`] backed by an RPC [`RootProvider`].
#[derive(Debug)]
pub struct RpcL1HeadProvider {
    provider: RootProvider,
}

impl RpcL1HeadProvider {
    /// Creates a new provider wrapping the given RPC root provider.
    pub const fn new(provider: RootProvider) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl L1HeadProvider for RpcL1HeadProvider {
    async fn finalized_head_hash(&self) -> eyre::Result<B256> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Finalized)
            .await?
            .ok_or_else(|| eyre::eyre!("L1 finalized block not found"))?;
        Ok(block.header.hash)
    }
}

/// Trait for sourcing proofs from a TEE enclave.
#[async_trait]
pub trait TeeProofProvider: Send + Sync + std::fmt::Debug {
    /// Sends a proof request to the TEE backend and returns the result.
    async fn prove(&self, request: ProofRequest) -> eyre::Result<ProofResult>;
}

/// [`TeeProofProvider`] backed by an [`EnclaveClient`] RPC connection.
#[derive(Debug)]
pub struct EnclaveTeeProvider {
    client: EnclaveClient,
}

impl EnclaveTeeProvider {
    /// Creates a new provider wrapping the given enclave client.
    pub const fn new(client: EnclaveClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl TeeProofProvider for EnclaveTeeProvider {
    async fn prove(&self, request: ProofRequest) -> eyre::Result<ProofResult> {
        self.client.prove(request).await.map_err(Into::into)
    }
}
