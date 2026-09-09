use std::time::Instant;

use alloy_eips::{BlockId, BlockNumberOrTag, eip7685::EMPTY_REQUESTS_HASH};
use alloy_primitives::{Address, B256};
use alloy_provider::{EthGetBlock, Provider, RootProvider};
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_common_network::{Ethereum, Network};
use base_common_types_payload::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelope, BaseExecutionPayloadSidecar,
    BasePayloadAttributes, CancunPayloadFields, ExecutionData, ForkchoiceState, ForkchoiceUpdated,
    PayloadId, PayloadStatus, PraguePayloadFields,
};
use base_consensus_providers::LocalL2Provider;
use base_execution_payload_builder::{BaseExecutionHandle, BasePayloadBuilderAttributes};
use base_protocol::L2BlockInfo;
use reth_network::NetworkHandle;
use reth_network_api::NetworkInfo;

use crate::{EngineClient, EngineClientError, Metrics};

/// Consensus access to the co-located execution node.
#[derive(Clone, Debug)]
pub struct LocalEngineClient {
    /// Remote L1 data source.
    pub l1: RootProvider,
    /// Local canonical chain and state.
    pub l2: LocalL2Provider,
    /// Execution driver and payload builder.
    pub execution: BaseExecutionHandle,
    /// Committed proofs-history progress, when that extension is enabled.
    pub proofs_progress: Option<base_execution_trie::ProofsProgress>,
    /// Execution synchronization status.
    pub network: NetworkHandle,
}

#[async_trait]
impl EngineClient for LocalEngineClient {
    fn cfg(&self) -> &RollupConfig {
        &self.l2.rollup_config
    }

    fn get_l1_block(&self, block: BlockId) -> EthGetBlock<<Ethereum as Network>::BlockResponse> {
        self.l1.get_block(block)
    }

    async fn get_l2_block(
        &self,
        id: BlockId,
    ) -> Result<Option<reth_primitives_traits::SealedBlock>, EngineClientError> {
        self.l2
            .block(id)
            .await
            .map(|block| block.map(reth_primitives_traits::SealedBlock::seal_slow))
            .map_err(EngineClientError::Local)
    }

    async fn storage_root(
        &self,
        address: Address,
        block: BlockId,
    ) -> Result<B256, EngineClientError> {
        self.l2.storage_root(block, address).await.map_err(EngineClientError::Local)
    }

    async fn l2_block_by_label(
        &self,
        tag: BlockNumberOrTag,
    ) -> Result<Option<reth_primitives_traits::SealedBlock>, EngineClientError> {
        Ok(self.get_l2_block(tag.into()).await?)
    }

    async fn l2_block_info_by_label(
        &self,
        tag: BlockNumberOrTag,
    ) -> Result<Option<L2BlockInfo>, EngineClientError> {
        self.l2.block_info(tag.into()).await.map_err(EngineClientError::Local)
    }

    async fn el_syncing(&self) -> Result<bool, EngineClientError> {
        Ok(self.network.is_syncing())
    }

    async fn submit_payload(
        &self,
        envelope: BaseExecutionPayloadEnvelope,
    ) -> Result<PayloadStatus, EngineClientError> {
        let started = Instant::now();
        let result = async {
            let sidecar = match envelope.parent_beacon_block_root {
                None => BaseExecutionPayloadSidecar::default(),
                Some(root) => {
                    let cancun = CancunPayloadFields::new(root, Vec::new());
                    if matches!(envelope.execution_payload, BaseExecutionPayload::V4(_)) {
                        BaseExecutionPayloadSidecar::v4(
                            cancun,
                            PraguePayloadFields::new(EMPTY_REQUESTS_HASH),
                        )
                    } else {
                        BaseExecutionPayloadSidecar::v3(cancun)
                    }
                }
            };
            self.execution
                .driver
                .new_payload(ExecutionData::new(envelope.execution_payload, sidecar, None))
                .await
                .map_err(Into::into)
        }
        .await;
        Metrics::engine_method_request_duration(Metrics::NEW_PAYLOAD_METHOD)
            .record(started.elapsed().as_secs_f64());
        result
    }

    async fn update_forkchoice(
        &self,
        state: ForkchoiceState,
        attributes: Option<BasePayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, EngineClientError> {
        let started = Instant::now();
        let result = async {
            let attributes = attributes
                .map(|attributes| {
                    BasePayloadBuilderAttributes::try_new(state.head_block_hash, attributes, 3)
                })
                .transpose()
                .map_err(|error| EngineClientError::InvalidAttributes(error.to_string()))?;
            self.execution.update_forkchoice(state, attributes).await.map_err(Into::into)
        }
        .await;
        Metrics::engine_method_request_duration(Metrics::FORKCHOICE_UPDATE_METHOD)
            .record(started.elapsed().as_secs_f64());
        result
    }

    async fn resolve_payload(
        &self,
        id: PayloadId,
    ) -> Result<BaseExecutionPayloadEnvelope, EngineClientError> {
        let started = Instant::now();
        let result = async {
            let built = self.execution.resolve_payload(id).await?;
            let block = built.block();
            let hash = block.hash();
            let parent_beacon_block_root = block.parent_beacon_block_root;
            let block = block.clone_block();
            let (execution_payload, _) = BaseExecutionPayload::from_block_unchecked(hash, &block);
            Ok(BaseExecutionPayloadEnvelope { execution_payload, parent_beacon_block_root })
        }
        .await;
        Metrics::engine_method_request_duration(Metrics::GET_PAYLOAD_METHOD)
            .record(started.elapsed().as_secs_f64());
        result
    }
}
