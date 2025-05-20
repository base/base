use super::DEFAULT_JWT_TOKEN;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ExecutionPayloadV3, ForkchoiceUpdated, PayloadStatus};
use jsonrpsee::{
    core::RpcResult,
    http_client::{transport::HttpBackend, HttpClient},
    proc_macros::rpc,
};
use reth::rpc::{api::EngineApiClient, types::engine::ForkchoiceState};
use reth_node_api::{EngineTypes, PayloadTypes};
use reth_optimism_node::OpEngineTypes;
use reth_payload_builder::PayloadId;
use reth_rpc_layer::{AuthClientLayer, AuthClientService, JwtSecret};
use serde_json::Value;
use std::str::FromStr;

/// Helper for engine api operations
pub struct EngineApi {
    pub engine_api_client: HttpClient<AuthClientService<HttpBackend>>,
}

/// Builder for EngineApi configuration
pub struct EngineApiBuilder {
    url: String,
    jwt_secret: String,
}

impl Default for EngineApiBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineApiBuilder {
    pub fn new() -> Self {
        Self {
            url: String::from("http://localhost:8551"),
            jwt_secret: String::from(DEFAULT_JWT_TOKEN),
        }
    }

    pub fn with_url(mut self, url: &str) -> Self {
        self.url = url.to_string();
        self
    }

    pub fn build(self) -> Result<EngineApi, Box<dyn std::error::Error>> {
        let secret_layer = AuthClientLayer::new(JwtSecret::from_str(&self.jwt_secret)?);
        let middleware = tower::ServiceBuilder::default().layer(secret_layer);
        let client = jsonrpsee::http_client::HttpClientBuilder::default()
            .set_http_middleware(middleware)
            .build(&self.url)
            .expect("Failed to create http client");

        Ok(EngineApi {
            engine_api_client: client,
        })
    }
}

impl EngineApi {
    pub fn builder() -> EngineApiBuilder {
        EngineApiBuilder::new()
    }

    pub fn new(url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        Self::builder().with_url(url).build()
    }

    pub fn new_with_port(port: u16) -> Result<Self, Box<dyn std::error::Error>> {
        Self::builder()
            .with_url(&format!("http://localhost:{port}"))
            .build()
    }

    pub async fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> eyre::Result<<OpEngineTypes as EngineTypes>::ExecutionPayloadEnvelopeV3> {
        println!(
            "Fetching payload with id: {} at {}",
            payload_id,
            chrono::Utc::now()
        );

        Ok(
            EngineApiClient::<OpEngineTypes>::get_payload_v3(&self.engine_api_client, payload_id)
                .await?,
        )
    }

    pub async fn new_payload(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> eyre::Result<PayloadStatus> {
        println!("Submitting new payload at {}...", chrono::Utc::now());

        Ok(EngineApiClient::<OpEngineTypes>::new_payload_v3(
            &self.engine_api_client,
            payload,
            versioned_hashes,
            parent_beacon_block_root,
        )
        .await?)
    }

    pub async fn update_forkchoice(
        &self,
        current_head: B256,
        new_head: B256,
        payload_attributes: Option<<OpEngineTypes as PayloadTypes>::PayloadAttributes>,
    ) -> eyre::Result<ForkchoiceUpdated> {
        println!("Updating forkchoice at {}...", chrono::Utc::now());

        Ok(EngineApiClient::<OpEngineTypes>::fork_choice_updated_v3(
            &self.engine_api_client,
            ForkchoiceState {
                head_block_hash: new_head,
                safe_block_hash: current_head,
                finalized_block_hash: current_head,
            },
            payload_attributes,
        )
        .await?)
    }

    pub async fn latest(&self) -> eyre::Result<Option<alloy_rpc_types_eth::Block>> {
        self.get_block_by_number(BlockNumberOrTag::Latest, false)
            .await
    }

    pub async fn get_block_by_number(
        &self,
        number: BlockNumberOrTag,
        include_txs: bool,
    ) -> eyre::Result<Option<alloy_rpc_types_eth::Block>> {
        Ok(
            BlockApiClient::get_block_by_number(&self.engine_api_client, number, include_txs)
                .await?,
        )
    }
}

#[rpc(server, client, namespace = "eth")]
pub trait BlockApi {
    #[method(name = "getBlockByNumber")]
    async fn get_block_by_number(
        &self,
        block_number: BlockNumberOrTag,
        include_txs: bool,
    ) -> RpcResult<Option<alloy_rpc_types_eth::Block>>;
}

pub async fn generate_genesis(output: Option<String>) -> eyre::Result<()> {
    // Read the template file
    let template = include_str!("artifacts/genesis.json.tmpl");

    // Parse the JSON
    let mut genesis: Value = serde_json::from_str(template)?;

    // Update the timestamp field - example using current timestamp
    let timestamp = chrono::Utc::now().timestamp();
    if let Some(config) = genesis.as_object_mut() {
        // Assuming timestamp is at the root level - adjust path as needed
        config["timestamp"] = Value::String(format!("0x{timestamp:x}"));
    }

    // Write the result to the output file
    if let Some(output) = output {
        std::fs::write(&output, serde_json::to_string_pretty(&genesis)?)?;
        println!("Generated genesis file at: {output}");
    } else {
        println!("{}", serde_json::to_string_pretty(&genesis)?);
    }

    Ok(())
}
