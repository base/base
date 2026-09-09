//! Historical proofs RPC server implementation for `debug_` namespace.

use std::sync::Arc;

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::B256;
use alloy_rpc_types_debug::ExecutionWitness;
use async_trait::async_trait;
use base_common_runtime_tasks::Runtime;
use base_common_types_chain::BlockHeader;
use base_common_types_payload::BasePayloadAttributes;
use base_execution_evm_blocks::{BaseEvmConfig, ExecutionWitnessRecord, Executor};
use base_execution_evm_runtime::database::State;
use base_execution_payload_builder::{
    BasePayloadBuilderAttributes, PayloadConfig,
    builder::{BasePayloadBuilderCtx, Builder},
};
use base_execution_payload_types::PayloadBuilderError;
use base_execution_state_tasks::{BaseProofsStorage, BaseProofsStore};
use base_execution_txpool::BasePooledTransaction;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::RpcResult;
use reth_payload_util::NoopPayloadTransactions;
use reth_primitives_traits::SealedHeader;
use reth_provider::{
    BlockReaderIdExt, ChainSpecProvider, HeaderProvider, ProviderError, ProviderResult,
    StateProviderFactory,
};
use reth_rpc_eth_types::EthApiError;
use reth_rpc_server_types::{ToRpcResult, result::internal_rpc_err};
use base_execution_state_types::ExecutionWitnessMode;
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, oneshot};

use crate::{
    BaseEthApi,
    metrics::{DebugApiExtMetrics, DebugApis},
    state::BaseStateProviderFactory,
};

/// Represents the current proofs sync status.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ProofsSyncStatus {
    /// The earliest block number for which proofs are available.
    earliest: Option<u64>,
    /// The latest block number for which proofs are available.
    latest: Option<u64>,
}

#[cfg_attr(not(test), rpc(server, namespace = "debug"))]
#[cfg_attr(test, rpc(server, client, namespace = "debug"))]
pub trait DebugApiOverride<Attributes> {
    /// Executes a payload and returns the execution witness.
    #[method(name = "executePayload")]
    async fn execute_payload(
        &self,
        parent_block_hash: B256,
        attributes: Attributes,
    ) -> RpcResult<ExecutionWitness>;

    /// Returns the execution witness for a given block.
    #[method(name = "executionWitness")]
    async fn execution_witness(&self, block: BlockNumberOrTag) -> RpcResult<ExecutionWitness>;

    /// Returns the current proofs sync status.
    #[method(name = "proofsSyncStatus")]
    async fn proofs_sync_status(&self) -> RpcResult<ProofsSyncStatus>;
}

#[derive(Debug)]
/// Overrides applied to the `debug_` namespace of the RPC API for the proofs `ExEx`.
pub struct DebugApiExt<Storage, Provider> {
    inner: Arc<DebugApiExtInner<Storage, Provider>>,
}

impl<Storage, Provider> DebugApiExt<Storage, Provider>
where
    Storage: BaseProofsStore + Clone + 'static,
    Provider: BlockReaderIdExt,
{
    /// Creates a new instance of the `DebugApiExt`.
    pub fn new(
        provider: Provider,
        eth_api: BaseEthApi,
        preimage_store: BaseProofsStorage<Storage>,
        task_spawner: Runtime,
        evm_config: BaseEvmConfig,
    ) -> Self {
        Self {
            inner: Arc::new(DebugApiExtInner::new(
                provider,
                eth_api,
                preimage_store,
                task_spawner,
                evm_config,
            )),
        }
    }
}

#[derive(Debug)]
/// Overrides applied to the `debug_` namespace of the RPC API for historical proofs `ExEx`.
pub struct DebugApiExtInner<Storage, Provider> {
    provider: Provider,
    eth_api: BaseEthApi,
    storage: BaseProofsStorage<Storage>,
    state_provider_factory: BaseStateProviderFactory<Storage>,
    evm_config: BaseEvmConfig,
    task_spawner: Runtime,
    semaphore: Semaphore,
}

impl<P, Provider> DebugApiExtInner<P, Provider>
where
    P: BaseProofsStore + Clone + 'static,
{
    fn new(
        provider: Provider,
        eth_api: BaseEthApi,
        storage: BaseProofsStorage<P>,
        task_spawner: Runtime,
        evm_config: BaseEvmConfig,
    ) -> Self {
        Self {
            provider,
            storage: storage.clone(),
            state_provider_factory: BaseStateProviderFactory::new(eth_api.clone(), storage),
            eth_api,
            evm_config,
            task_spawner,
            semaphore: Semaphore::new(3),
        }
    }
}

impl<P, Provider> DebugApiExt<P, Provider>
where
    P: BaseProofsStore + Clone + 'static,
    Provider: BlockReaderIdExt + HeaderProvider,
{
    fn parent_header(&self, parent_block_hash: B256) -> ProviderResult<SealedHeader> {
        self.inner
            .provider
            .sealed_header_by_hash(parent_block_hash)?
            .ok_or_else(|| ProviderError::HeaderNotFound(parent_block_hash.into()))
    }
}

#[async_trait]
impl<P, Provider> DebugApiOverrideServer<BasePayloadAttributes> for DebugApiExt<P, Provider>
where
    P: BaseProofsStore + Clone + 'static,
    Provider: BlockReaderIdExt
        + StateProviderFactory
        + ChainSpecProvider
        + HeaderProvider
        + Clone
        + 'static,
{
    async fn execute_payload(
        &self,
        parent_block_hash: B256,
        attributes: BasePayloadAttributes,
    ) -> RpcResult<ExecutionWitness> {
        DebugApiExtMetrics::record_operation_async(DebugApis::DebugExecutePayload, async {
            let _permit = self.inner.semaphore.acquire().await;

            let parent_header = self.parent_header(parent_block_hash).to_rpc_result()?;

            let (tx, rx) = oneshot::channel();
            let this = Arc::clone(&self.inner);
            let eth_api = self.inner.eth_api.provider().clone();
            self.inner.task_spawner.spawn_blocking_task(async move {
                let result = async {
                    let parent_hash = parent_header.hash();
                    let attributes =
                        BasePayloadBuilderAttributes::try_new(parent_hash, attributes, 3)
                            .map_err(PayloadBuilderError::other)?;
                    let payload_id = attributes.payload_attributes.id;

                    let config =
                        PayloadConfig::new(Arc::new(parent_header), attributes, payload_id);
                    let ctx = BasePayloadBuilderCtx {
                        evm_config: this.evm_config.clone(),
                        chain_spec: this.provider.chain_spec(),
                        config,
                        cancel: Default::default(),
                        best_payload: Default::default(),
                        builder_config: Default::default(),
                    };

                    let state_provider = this
                        .state_provider_factory
                        .state_provider(Some(BlockId::Hash(parent_hash.into())))
                        .await
                        .map_err(PayloadBuilderError::other)?;

                    let builder = Builder::new(|_| {
                        NoopPayloadTransactions::<BasePooledTransaction>::default()
                    });

                    builder
                        .witness(state_provider, eth_api, &ctx)
                        .map_err(PayloadBuilderError::other)
                };

                let _ = tx.send(result.await);
            });

            rx.await
                .map_err(|err| internal_rpc_err(err.to_string()))?
                .map_err(|err| internal_rpc_err(err.to_string()))
        })
        .await
    }

    async fn execution_witness(&self, block_id: BlockNumberOrTag) -> RpcResult<ExecutionWitness> {
        DebugApiExtMetrics::record_operation_async(DebugApis::DebugExecutionWitness, async {
            let _permit = self.inner.semaphore.acquire().await;

            let block = self
                .inner
                .eth_api
                .recovered_block(block_id.into())
                .await?
                .ok_or(EthApiError::HeaderNotFound(block_id.into()))?;

            let this = Arc::clone(&self.inner);
            let block_number = block.header().number();

            let state_provider = this
                .state_provider_factory
                .state_provider(Some(BlockId::Number(block.parent_num_hash().number.into())))
                .await
                .map_err(EthApiError::from)?;
            let db = state_provider.as_ref();
            let block_executor = this.eth_api.evm_config().executor(db);

            let mut witness = None;

            let mode = ExecutionWitnessMode::default();
            let _ = block_executor
                .execute_with_state_closure(&block, |statedb: &State<_>| {
                    witness = Some(ExecutionWitnessRecord::new(statedb).into_execution_witness(
                        &statedb.database,
                        self.inner.eth_api.provider(),
                        block_number,
                        mode,
                    ));
                })
                .map_err(EthApiError::from)?;

            let witness = witness.unwrap().map_err(EthApiError::from)?;

            Ok(witness)
        })
        .await
    }

    async fn proofs_sync_status(&self) -> RpcResult<ProofsSyncStatus> {
        let earliest = self
            .inner
            .storage
            .get_earliest_block_number()
            .map_err(|err| internal_rpc_err(err.to_string()))?;
        let latest = self
            .inner
            .storage
            .get_latest_block_number()
            .map_err(|err| internal_rpc_err(err.to_string()))?;

        Ok(ProofsSyncStatus {
            earliest: earliest.map(|(block_number, _)| block_number),
            latest: latest.map(|(block_number, _)| block_number),
        })
    }
}
