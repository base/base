//! Wire access used only to compare blocks against an external reference client.

use alloy_eips::eip7685::Requests;
use alloy_primitives::B256;
use base_common_client_ethereum::AuthClientLayer;
use base_common_types_payload::JwtSecret;
use base_common_types_payload::{
    BaseExecutionPayloadV4, ForkchoiceState, ForkchoiceUpdated, PayloadStatus,
};
use base_execution_payload_types::BasePayloadBuilderAttributes;
use jsonrpsee::{core::client::ClientT, rpc_params};

/// Access to the separately maintained reference client's execution endpoint.
#[derive(Debug)]
pub struct ExternalEngineApi {
    /// Reference client HTTP endpoint.
    pub url: String,
    /// Authentication for the separately maintained reference client.
    pub secret: JwtSecret,
}

impl ExternalEngineApi {
    /// Builds an authenticated HTTP client for the external reference node.
    pub fn client(&self) -> eyre::Result<impl ClientT> {
        let middleware = tower::ServiceBuilder::default().layer(AuthClientLayer::new(self.secret));
        Ok(jsonrpsee::http_client::HttpClientBuilder::default()
            .set_http_middleware(middleware)
            .build(&self.url)?)
    }

    /// Submits a payload to the external reference client.
    pub async fn new_payload(
        &self,
        payload: BaseExecutionPayloadV4,
        hashes: Vec<B256>,
        root: B256,
        requests: Requests,
    ) -> eyre::Result<PayloadStatus> {
        let client = self.client()?;
        Ok(client
            .request("engine_newPayloadV4", rpc_params![payload, hashes, root, requests])
            .await?)
    }

    /// Advances the external reference client's canonical head.
    pub async fn update_forkchoice(
        &self,
        current: B256,
        head: B256,
        attributes: Option<BasePayloadBuilderAttributes>,
    ) -> eyre::Result<ForkchoiceUpdated> {
        let client = self.client()?;
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
