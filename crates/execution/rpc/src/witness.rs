//! Support for Base-specific witness RPCs.

use std::{fmt::Debug, sync::Arc};

use alloy_primitives::B256;
use alloy_rpc_types_debug::ExecutionWitness;
use base_common_runtime::Runtime;
use base_common_types_chain::SealedHeader;
use base_common_types_payload::BasePayloadAttributes;
use base_execution_payload::BasePayloadBuilder;
use base_execution_state_types::{HeaderProvider, ProviderError, ProviderResult};
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::{RpcResult, async_trait};
use tokio::sync::{Semaphore, oneshot};

use crate::RpcErrorFactory;

#[cfg_attr(not(test), rpc(server, namespace = "debug"))]
#[cfg_attr(test, rpc(server, client, namespace = "debug"))]
/// RPC trait for the `debug_executePayload` endpoint.
pub trait DebugExecutionWitnessApi {
    /// Executes a payload and returns the execution witness.
    #[method(name = "executePayload")]
    async fn execute_payload(
        &self,
        parent_block_hash: B256,
        attributes: BasePayloadAttributes,
    ) -> RpcResult<ExecutionWitness>;
}

/// An extension to the `debug_` namespace of the RPC API.
pub struct BaseDebugWitnessApi {
    inner: Arc<BaseDebugWitnessApiInner>,
}

impl BaseDebugWitnessApi {
    /// Creates a new instance of the `BaseDebugWitnessApi`.
    pub fn new(
        provider: base_execution_state_provider::BlockchainProvider,
        task_spawner: Runtime,
        builder: BasePayloadBuilder,
    ) -> Self {
        let semaphore = Arc::new(Semaphore::new(3));
        let inner = BaseDebugWitnessApiInner { provider, builder, task_spawner, semaphore };
        Self { inner: Arc::new(inner) }
    }
}

impl BaseDebugWitnessApi {
    /// Fetches the parent header by hash.
    fn parent_header(&self, parent_block_hash: B256) -> ProviderResult<SealedHeader> {
        self.inner
            .provider
            .sealed_header_by_hash(parent_block_hash)?
            .ok_or_else(|| ProviderError::HeaderNotFound(parent_block_hash.into()))
    }
}

#[async_trait]
impl DebugExecutionWitnessApiServer for BaseDebugWitnessApi {
    async fn execute_payload(
        &self,
        parent_block_hash: B256,
        attributes: BasePayloadAttributes,
    ) -> RpcResult<ExecutionWitness> {
        let _permit = self.inner.semaphore.acquire().await;

        let parent_header = self
            .parent_header(parent_block_hash)
            .map_err(|err| crate::RpcErrorFactory::internal(err.to_string()))?;

        let (tx, rx) = oneshot::channel();
        let this = self.clone();
        self.inner.task_spawner.spawn_blocking_task(async move {
            let res = this.inner.builder.payload_witness(parent_header, attributes);
            let _ = tx.send(res);
        });

        rx.await
            .map_err(|err| RpcErrorFactory::internal(err.to_string()))?
            .map_err(|err| RpcErrorFactory::internal(err.to_string()))
    }
}

impl Clone for BaseDebugWitnessApi {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}
impl Debug for BaseDebugWitnessApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseDebugWitnessApi").finish_non_exhaustive()
    }
}

struct BaseDebugWitnessApiInner {
    provider: base_execution_state_provider::BlockchainProvider,
    builder: BasePayloadBuilder,
    task_spawner: Runtime,
    semaphore: Arc<Semaphore>,
}
