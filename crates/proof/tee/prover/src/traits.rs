use async_trait::async_trait;
use base_enclave::{AggregateRequest, ExecutionWitness, Proposal};
use base_enclave_client::{ClientError, EnclaveClient, ExecuteStatelessRequest};
use base_proof_rpc::{L2Provider, RpcResult};

/// Trait for executing stateless block validation and aggregation in a TEE.
///
/// Both the proposer and challenger need `execute_stateless` and `aggregate`.
#[async_trait]
pub trait TeeExecutor: Send + Sync {
    /// Executes stateless block validation in the enclave.
    async fn execute_stateless(
        &self,
        req: ExecuteStatelessRequest,
    ) -> Result<Proposal, ClientError>;

    /// Aggregates multiple proposals into a single batched proposal.
    async fn aggregate(&self, req: AggregateRequest) -> Result<Proposal, ClientError>;
}

/// Blanket implementation for [`EnclaveClient`].
#[async_trait]
impl TeeExecutor for EnclaveClient {
    async fn execute_stateless(
        &self,
        req: ExecuteStatelessRequest,
    ) -> Result<Proposal, ClientError> {
        self.execute_stateless(req).await
    }

    async fn aggregate(&self, req: AggregateRequest) -> Result<Proposal, ClientError> {
        self.aggregate(req).await
    }
}

/// Extension trait for L2 providers that support execution witness retrieval.
///
/// The base [`L2Provider`] trait does not include `debug_executionWitness`
/// because it is only needed for proof generation, not general L2 queries.
#[async_trait]
pub trait ExecutionWitnessProvider: L2Provider {
    /// Gets the execution witness for a block via `debug_executionWitness`.
    async fn execution_witness(&self, block_number: u64) -> RpcResult<ExecutionWitness>;
}
