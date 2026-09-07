//! Support for Base-specific witness RPCs.

use std::{fmt::Debug, sync::Arc};

use alloy_primitives::B256;
use alloy_rpc_types_debug::ExecutionWitness;
use base_common_consensus::BaseTxEnvelope;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_execution_chainspec::{BaseChainSpec, ChainSpecProvider};
use base_execution_payload_builder::{BasePayloadBuilder, BasePayloadBuilderAttributes};
use base_execution_txpool::BasePooledTx;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::{RpcResult, async_trait};
use reth_evm::ConfigureEvm;
use reth_node_api::BuildNextEnv;
use reth_primitives_traits::SealedHeader;
use reth_rpc_server_types::{ToRpcResult, result::internal_rpc_err};
use reth_storage_api::{
    BlockReaderIdExt, StateProviderFactory,
    errors::{ProviderError, ProviderResult},
};
use reth_tasks::Runtime;
use reth_transaction_pool::TransactionPool;
use tokio::sync::{Semaphore, oneshot};

#[cfg_attr(not(test), rpc(server, namespace = "debug"))]
#[cfg_attr(test, rpc(server, client, namespace = "debug"))]
/// RPC trait for the `debug_executePayload` endpoint.
pub trait DebugExecutionWitnessApi<Attributes> {
    /// Executes a payload and returns the execution witness.
    #[method(name = "executePayload")]
    async fn execute_payload(
        &self,
        parent_block_hash: B256,
        attributes: Attributes,
    ) -> RpcResult<ExecutionWitness>;
}

/// An extension to the `debug_` namespace of the RPC API.
pub struct BaseDebugWitnessApi<Pool, Provider, EvmConfig> {
    inner: Arc<BaseDebugWitnessApiInner<Pool, Provider, EvmConfig>>,
}

impl<Pool, Provider, EvmConfig> BaseDebugWitnessApi<Pool, Provider, EvmConfig> {
    /// Creates a new instance of the `BaseDebugWitnessApi`.
    pub fn new(
        provider: Provider,
        task_spawner: Runtime,
        builder: BasePayloadBuilder<Pool, Provider, EvmConfig, ()>,
    ) -> Self {
        let semaphore = Arc::new(Semaphore::new(3));
        let inner = BaseDebugWitnessApiInner { provider, builder, task_spawner, semaphore };
        Self { inner: Arc::new(inner) }
    }
}

impl<Pool, Provider, EvmConfig> BaseDebugWitnessApi<Pool, Provider, EvmConfig>
where
    EvmConfig: ConfigureEvm,
    Provider: BlockReaderIdExt,
{
    /// Fetches the parent header by hash.
    fn parent_header(
        &self,
        parent_block_hash: B256,
    ) -> ProviderResult<SealedHeader<Provider::Header>> {
        self.inner
            .provider
            .sealed_header_by_hash(parent_block_hash)?
            .ok_or_else(|| ProviderError::HeaderNotFound(parent_block_hash.into()))
    }
}

#[async_trait]
impl<Pool, Provider, EvmConfig> DebugExecutionWitnessApiServer<BasePayloadAttributes>
    for BaseDebugWitnessApi<Pool, Provider, EvmConfig>
where
    Pool: TransactionPool<Transaction: BasePooledTx<Consensus = BaseTxEnvelope>> + 'static,
    Provider: BlockReaderIdExt<Header = alloy_consensus::Header>
        + StateProviderFactory
        + ChainSpecProvider
        + Clone
        + 'static,
    EvmConfig: ConfigureEvm<
            NextBlockEnvCtx: BuildNextEnv<
                BasePayloadBuilderAttributes<BaseTxEnvelope>,
                Provider::Header,
                BaseChainSpec,
            >,
        > + 'static,
{
    async fn execute_payload(
        &self,
        parent_block_hash: B256,
        attributes: BasePayloadAttributes,
    ) -> RpcResult<ExecutionWitness> {
        let _permit = self.inner.semaphore.acquire().await;

        let parent_header = self.parent_header(parent_block_hash).to_rpc_result()?;

        let (tx, rx) = oneshot::channel();
        let this = self.clone();
        self.inner.task_spawner.spawn_blocking_task(async move {
            let res = this.inner.builder.payload_witness(parent_header, attributes);
            let _ = tx.send(res);
        });

        rx.await
            .map_err(|err| internal_rpc_err(err.to_string()))?
            .map_err(|err| internal_rpc_err(err.to_string()))
    }
}

impl<Pool, Provider, EvmConfig> Clone for BaseDebugWitnessApi<Pool, Provider, EvmConfig> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}
impl<Pool, Provider, EvmConfig> Debug for BaseDebugWitnessApi<Pool, Provider, EvmConfig> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseDebugWitnessApi").finish_non_exhaustive()
    }
}

struct BaseDebugWitnessApiInner<Pool, Provider, EvmConfig> {
    provider: Provider,
    builder: BasePayloadBuilder<Pool, Provider, EvmConfig, ()>,
    task_spawner: Runtime,
    semaphore: Arc<Semaphore>,
}
