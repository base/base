//! Wire access used only to compare blocks against an external reference client.

use alloy_eips::eip7685::Requests;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated, PayloadStatus};
use base_common_consensus::BaseTxEnvelope;
use base_common_rpc_types_engine::BaseExecutionPayloadV4;
use base_execution_payload_types::BasePayloadBuilderAttributes;
use jsonrpsee::{core::client::ClientT, rpc_params};

/// Access to the separately maintained reference client's execution endpoint.
#[derive(Debug)]
pub struct ExternalEngineApi {
    /// Reference client socket.
    pub path: String,
}

impl ExternalEngineApi {
    /// Submits a payload to the external reference client.
    pub async fn new_payload(
        &self,
        payload: BaseExecutionPayloadV4,
        hashes: Vec<B256>,
        root: B256,
        requests: Requests,
    ) -> eyre::Result<PayloadStatus> {
        let client = reth_ipc::client::IpcClientBuilder::default().build(&self.path).await?;
        Ok(client
            .request("engine_newPayloadV4", rpc_params![payload, hashes, root, requests])
            .await?)
    }

    /// Advances the external reference client's canonical head.
    pub async fn update_forkchoice(
        &self,
        current: B256,
        head: B256,
        attributes: Option<BasePayloadBuilderAttributes<BaseTxEnvelope>>,
    ) -> eyre::Result<ForkchoiceUpdated> {
        let client = reth_ipc::client::IpcClientBuilder::default().build(&self.path).await?;
        Ok(client
            .request(
                "engine_forkchoiceUpdatedV3",
                rpc_params![
                    ForkchoiceState {
                        head_block_hash: head,
                        safe_block_hash: current,
                        finalized_block_hash: current
                    },
                    attributes
                ],
            )
            .await?)
    }
}
