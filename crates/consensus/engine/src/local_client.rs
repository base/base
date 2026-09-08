use alloy_eips::{BlockId, BlockNumberOrTag, eip7685::EMPTY_REQUESTS_HASH};
use alloy_primitives::{Address, B256};
use alloy_provider::{EthGetBlock, Provider, RootProvider};
use alloy_rpc_types_engine::{
    CancunPayloadFields, ForkchoiceState, ForkchoiceUpdated, PayloadId, PayloadStatus,
    PraguePayloadFields,
};
use alloy_transport::{TransportErrorKind, TransportResult};
use async_trait::async_trait;
use base_common_consensus::{BaseTransactionInfo, transaction::Recovered};
use base_common_genesis::RollupConfig;
use base_common_network::{Ethereum, Network};
use base_common_rpc_types::{Base, Transaction};
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelope, BaseExecutionPayloadSidecar,
    BasePayloadAttributes, ExecutionData,
};
use base_consensus_providers::LocalL2Provider;
use base_execution_payload_builder::{BaseExecutionHandle, BasePayloadBuilderAttributes};
use base_protocol::L2BlockInfo;
use reth_network::NetworkHandle;
use reth_network_api::NetworkInfo;
use reth_provider::{BlockReaderIdExt, TransactionVariant};

use crate::{EngineClient, EngineClientError};

/// Consensus access to the co-located execution node.
#[derive(Clone, Debug)]
pub struct LocalEngineClient {
    /// Remote L1 data source.
    pub l1: RootProvider,
    /// Local canonical chain and state.
    pub l2: LocalL2Provider,
    /// Execution driver and payload builder.
    pub execution: BaseExecutionHandle,
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
    ) -> TransportResult<Option<<Base as Network>::BlockResponse>> {
        // Retain the consensus reader's response shape while its callers migrate to native blocks.
        self.l2
            .read(move |provider| {
                let Some(block) =
                    provider.block_with_senders_by_id(id, TransactionVariant::WithHash)?
                else {
                    return Ok(None);
                };
                let (block, senders) = block.split();
                let mut senders = senders.into_iter();
                let rpc = alloy_rpc_types_eth::Block::from_consensus(block, None).map_transactions(
                    |tx| {
                        Transaction::from_transaction(
                            Recovered::new_unchecked(
                                tx,
                                senders.next().expect("one sender per transaction"),
                            ),
                            BaseTransactionInfo::default(),
                        )
                    },
                );
                Ok(Some(rpc.map_header(Into::into)))
            })
            .await
            .map_err(|error| TransportErrorKind::custom(error).into())
    }

    async fn storage_root(&self, address: Address, block: BlockId) -> TransportResult<B256> {
        self.l2
            .storage_root(block, address)
            .await
            .map_err(|error| TransportErrorKind::custom(error).into())
    }

    async fn l2_block_by_label(
        &self,
        tag: BlockNumberOrTag,
    ) -> Result<Option<<Base as Network>::BlockResponse>, EngineClientError> {
        Ok(self.get_l2_block(tag.into()).await?)
    }

    async fn l2_block_info_by_label(
        &self,
        tag: BlockNumberOrTag,
    ) -> Result<Option<L2BlockInfo>, EngineClientError> {
        self.l2
            .block_info(tag.into())
            .await
            .map_err(|error| EngineClientError::RpcError(TransportErrorKind::custom(error).into()))
    }

    async fn el_syncing(&self) -> Result<bool, EngineClientError> {
        Ok(self.network.is_syncing())
    }

    async fn submit_payload(
        &self,
        envelope: BaseExecutionPayloadEnvelope,
    ) -> Result<PayloadStatus, EngineClientError> {
        let sidecar = match &envelope.execution_payload {
            BaseExecutionPayload::V1(_) | BaseExecutionPayload::V2(_) => {
                BaseExecutionPayloadSidecar::default()
            }
            BaseExecutionPayload::V3(_) => {
                BaseExecutionPayloadSidecar::v3(CancunPayloadFields::new(
                    envelope.parent_beacon_block_root.unwrap_or_default(),
                    Vec::new(),
                ))
            }
            BaseExecutionPayload::V4(_) => BaseExecutionPayloadSidecar::v4(
                CancunPayloadFields::new(
                    envelope.parent_beacon_block_root.unwrap_or_default(),
                    Vec::new(),
                ),
                PraguePayloadFields::new(EMPTY_REQUESTS_HASH),
            ),
        };
        self.execution
            .driver
            .new_payload(ExecutionData::new(envelope.execution_payload, sidecar, None))
            .await
            .map_err(Into::into)
    }

    async fn update_forkchoice(
        &self,
        state: ForkchoiceState,
        attributes: Option<BasePayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, EngineClientError> {
        let attributes = attributes
            .map(|attributes| {
                BasePayloadBuilderAttributes::try_new(state.head_block_hash, attributes, 3)
            })
            .transpose()
            .map_err(|error| EngineClientError::InvalidAttributes(error.to_string()))?;
        self.execution.update_forkchoice(state, attributes).await.map_err(Into::into)
    }

    async fn resolve_payload(
        &self,
        id: PayloadId,
        _attributes: &BasePayloadAttributes,
    ) -> Result<BaseExecutionPayloadEnvelope, EngineClientError> {
        let built = self.execution.resolve_payload(id).await?;
        let block = built.block();
        let hash = block.hash();
        let parent_beacon_block_root = block.parent_beacon_block_root;
        let block = block.clone_block();
        let (execution_payload, _) = BaseExecutionPayload::from_block_unchecked(hash, &block);
        Ok(BaseExecutionPayloadEnvelope { execution_payload, parent_beacon_block_root })
    }
}
