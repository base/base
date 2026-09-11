use std::time::Instant;

use alloy_eips::{BlockId, BlockNumberOrTag, eip7685::EMPTY_REQUESTS_HASH};
use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use base_common_chain_config::RollupConfig;
use base_common_client_ethereum::{EthGetBlock, Ethereum, Network, Provider, RootProvider};
use base_common_types_payload::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelope, BaseExecutionPayloadSidecar,
    BasePayloadAttributes, CancunPayloadFields, ExecutionData, ForkchoiceState, PayloadId,
    PraguePayloadFields,
};
use base_consensus_batch::L2BlockInfo;
use base_consensus_source::LocalL2Provider;
use base_execution_engine_driver::BaseExecutionHandle;
use base_execution_network_service::{NetworkHandle, NetworkInfo};
use base_execution_payload::BasePayloadBuilderAttributes;

use crate::engine::{EngineClient, EngineClientError, Metrics};

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
    pub proofs_progress: Option<base_execution_state_operations::ProofsProgress>,
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
    ) -> Result<Option<base_common_types_chain::SealedBlock>, EngineClientError> {
        self.l2
            .block(id)
            .await
            .map(|block| block.map(base_common_types_chain::SealedBlock::seal_slow))
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
    ) -> Result<Option<base_common_types_chain::SealedBlock>, EngineClientError> {
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

    async fn append_payload(
        &self,
        envelope: BaseExecutionPayloadEnvelope,
        heads: ForkchoiceState,
    ) -> Result<base_common_types_payload::HeadUpdateOutcome, EngineClientError> {
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
                .append_payload(
                    ExecutionData::new(envelope.execution_payload, sidecar, None),
                    heads,
                )
                .await
                .map_err(Into::into)
        }
        .await;
        Metrics::engine_method_request_duration(Metrics::APPEND_PAYLOAD_METHOD)
            .record(started.elapsed().as_secs_f64());
        result
    }

    async fn update_heads(
        &self,
        heads: ForkchoiceState,
    ) -> Result<base_common_types_payload::HeadUpdateOutcome, EngineClientError> {
        let started = Instant::now();
        let result = self
            .execution
            .driver
            .update_heads(heads)
            .await
            .map_err(base_execution_engine_driver::ExecutionCommandError::from);
        Metrics::engine_method_request_duration(Metrics::UPDATE_HEADS_METHOD)
            .record(started.elapsed().as_secs_f64());
        Ok(result?)
    }

    async fn start_building(
        &self,
        heads: ForkchoiceState,
        attributes: BasePayloadAttributes,
    ) -> Result<PayloadId, EngineClientError> {
        let attributes =
            BasePayloadBuilderAttributes::try_new(heads.head_block_hash, attributes, 3)
                .map_err(|error| EngineClientError::InvalidAttributes(error.to_string()))?;
        let started = Instant::now();
        let result = self.execution.start_building(heads, attributes).await;
        Metrics::engine_method_request_duration(Metrics::START_BUILDING_METHOD)
            .record(started.elapsed().as_secs_f64());
        Ok(result?)
    }

    async fn end_building(
        &self,
        id: PayloadId,
    ) -> Result<BaseExecutionPayloadEnvelope, EngineClientError> {
        let started = Instant::now();
        let result = async {
            let built = self.execution.end_building(id).await?;
            let block = built.block();
            let hash = block.hash();
            let parent_beacon_block_root = block.parent_beacon_block_root;
            let block = block.clone_block();
            let (execution_payload, _) = BaseExecutionPayload::from_block_unchecked(hash, &block);
            Ok(BaseExecutionPayloadEnvelope { execution_payload, parent_beacon_block_root })
        }
        .await;
        Metrics::engine_method_request_duration(Metrics::END_BUILDING_METHOD)
            .record(started.elapsed().as_secs_f64());
        result
    }
}
