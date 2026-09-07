//! Support for Base-specific witness RPCs.

use std::{fmt::Debug, sync::Arc};

use alloy_primitives::B256;
use alloy_rpc_types_debug::ExecutionWitness;
use base_common_chains::Upgrades;
use base_common_consensus::BaseTxEnvelope;
use base_execution_payload_builder::{Attributes, BasePayloadBuilder};
use base_execution_txpool::BasePooledTx;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::{RpcResult, async_trait};
use reth_chainspec::ChainSpecProvider;
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
pub struct BaseDebugWitnessApi<Pool, Provider, EvmConfig, Attrs> {
    inner: Arc<BaseDebugWitnessApiInner<Pool, Provider, EvmConfig, Attrs>>,
}

impl<Pool, Provider, EvmConfig, Attrs> BaseDebugWitnessApi<Pool, Provider, EvmConfig, Attrs> {
    /// Creates a new instance of the `BaseDebugWitnessApi`.
    pub fn new(
        provider: Provider,
        task_spawner: Runtime,
        builder: BasePayloadBuilder<Pool, Provider, EvmConfig, (), Attrs>,
    ) -> Self {
        let semaphore = Arc::new(Semaphore::new(3));
        let inner = BaseDebugWitnessApiInner { provider, builder, task_spawner, semaphore };
        Self { inner: Arc::new(inner) }
    }
}

impl<Pool, Provider, EvmConfig, Attrs> BaseDebugWitnessApi<Pool, Provider, EvmConfig, Attrs>
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
impl<Pool, Provider, EvmConfig, Attrs> DebugExecutionWitnessApiServer<Attrs::RpcPayloadAttributes>
    for BaseDebugWitnessApi<Pool, Provider, EvmConfig, Attrs>
where
    Pool: TransactionPool<Transaction: BasePooledTx<Consensus = BaseTxEnvelope>> + 'static,
    Provider: BlockReaderIdExt<Header = alloy_consensus::Header>
        + StateProviderFactory
        + ChainSpecProvider<ChainSpec: Upgrades>
        + Clone
        + 'static,
    EvmConfig: ConfigureEvm<NextBlockEnvCtx: BuildNextEnv<Attrs, Provider::Header, Provider::ChainSpec>>
        + 'static,
    Attrs: Attributes<Transaction = BaseTxEnvelope>,
    Attrs::RpcPayloadAttributes: Send + Sync + 'static,
{
    async fn execute_payload(
        &self,
        parent_block_hash: B256,
        attributes: Attrs::RpcPayloadAttributes,
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

impl<Pool, Provider, EvmConfig, Attrs> Clone
    for BaseDebugWitnessApi<Pool, Provider, EvmConfig, Attrs>
{
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}
impl<Pool, Provider, EvmConfig, Attrs> Debug
    for BaseDebugWitnessApi<Pool, Provider, EvmConfig, Attrs>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseDebugWitnessApi").finish_non_exhaustive()
    }
}

struct BaseDebugWitnessApiInner<Pool, Provider, EvmConfig, Attrs> {
    provider: Provider,
    builder: BasePayloadBuilder<Pool, Provider, EvmConfig, (), Attrs>,
    task_spawner: Runtime,
    semaphore: Arc<Semaphore>,
}
