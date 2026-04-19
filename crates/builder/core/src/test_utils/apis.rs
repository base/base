use core::{future::Future, marker::PhantomData};

use alloy_eips::{BlockNumberOrTag, eip7685::Requests};
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated, PayloadStatus};
use base_common_rpc_types_engine::BaseExecutionPayloadV4;
use base_execution_rpc::BaseEngineApiClient;
use base_node_core::BaseEngineTypes;
use jsonrpsee::{
    core::{RpcResult, client::SubscriptionClientT},
    proc_macros::rpc,
};
use reth_node_api::{EngineTypes, PayloadTypes};
use reth_payload_builder::PayloadId;
use reth_rpc_layer::{AuthClientLayer, JwtSecret};
use serde_json::Value;
use tracing::{debug, info};

use super::DEFAULT_JWT_TOKEN;

/// RPC transport address for connecting to an execution client.
#[derive(Clone, Debug)]
pub enum Address {
    /// Unix IPC socket path.
    Ipc(String),
    /// HTTP(S) URL endpoint.
    Http(url::Url),
}

/// Abstraction over RPC transport protocols (IPC, HTTP) for Engine API clients.
pub trait Protocol {
    /// Creates a new JSON-RPC client connected via this protocol.
    fn client(
        jwt: JwtSecret,
        address: Address,
    ) -> impl Future<Output = impl SubscriptionClientT + Send + Sync + Unpin + 'static>;
}

/// HTTP transport protocol marker.
#[derive(Debug)]
pub struct Http;
impl Protocol for Http {
    async fn client(
        jwt: JwtSecret,
        address: Address,
    ) -> impl SubscriptionClientT + Send + Sync + Unpin + 'static {
        let Address::Http(url) = address else {
            unreachable!();
        };

        let secret_layer = AuthClientLayer::new(jwt);
        let middleware = tower::ServiceBuilder::default().layer(secret_layer);
        jsonrpsee::http_client::HttpClientBuilder::default()
            .set_http_middleware(middleware)
            .build(url)
            .expect("Failed to create http client")
    }
}

/// IPC transport protocol marker.
#[derive(Debug)]
pub struct Ipc;
impl Protocol for Ipc {
    async fn client(
        _: JwtSecret, // ipc does not use JWT
        address: Address,
    ) -> impl SubscriptionClientT + Send + Sync + Unpin + 'static {
        let Address::Ipc(path) = address else {
            unreachable!();
        };
        reth_ipc::client::IpcClientBuilder::default()
            .build(&path)
            .await
            .expect("Failed to create ipc client")
    }
}

/// Helper for engine api operations
#[derive(Debug)]
pub struct EngineApi<P: Protocol = Ipc> {
    address: Address,
    jwt_secret: JwtSecret,
    _tag: PhantomData<P>,
}

impl<P: Protocol> EngineApi<P> {
    async fn client(&self) -> impl SubscriptionClientT + Send + Sync + Unpin + 'static {
        P::client(self.jwt_secret, self.address.clone()).await
    }
}

// http specific
impl EngineApi<Http> {
    /// Creates an HTTP [`EngineApi`] client from a URL string.
    pub fn with_http(url: &str) -> Self {
        Self {
            address: Address::Http(url.parse().expect("Invalid URL")),
            jwt_secret: DEFAULT_JWT_TOKEN.parse().expect("Invalid JWT"),
            _tag: PhantomData,
        }
    }

    /// Creates an HTTP [`EngineApi`] client targeting `localhost` on the given port.
    pub fn with_localhost_port(port: u16) -> Self {
        Self {
            address: Address::Http(
                format!("http://localhost:{port}").parse().expect("Invalid URL"),
            ),
            jwt_secret: DEFAULT_JWT_TOKEN.parse().expect("Invalid JWT"),
            _tag: PhantomData,
        }
    }

    /// Overrides the port on this client's URL.
    pub fn with_port(mut self, port: u16) -> Self {
        let Address::Http(url) = &mut self.address else {
            unreachable!();
        };

        url.set_port(Some(port)).expect("Invalid port");
        self
    }

    /// Overrides the JWT secret used for authentication.
    pub fn with_jwt_secret(mut self, jwt_secret: &str) -> Self {
        self.jwt_secret = jwt_secret.parse().expect("Invalid JWT");
        self
    }

    /// Returns a reference to the underlying HTTP URL.
    pub fn url(&self) -> &url::Url {
        let Address::Http(url) = &self.address else {
            unreachable!();
        };
        url
    }
}

// ipc specific
impl EngineApi<Ipc> {
    /// Creates an IPC [`EngineApi`] client from a socket path.
    pub fn with_ipc(path: &str) -> Self {
        Self {
            address: Address::Ipc(path.into()),
            jwt_secret: DEFAULT_JWT_TOKEN.parse().expect("Invalid JWT"),
            _tag: PhantomData,
        }
    }

    /// Returns the IPC socket path.
    pub fn path(&self) -> &str {
        let Address::Ipc(path) = &self.address else {
            unreachable!();
        };
        path
    }
}

impl<P: Protocol> EngineApi<P> {
    /// Fetches an execution payload by its identifier.
    pub async fn get_payload(
        &self,
        payload_id: PayloadId,
    ) -> eyre::Result<<BaseEngineTypes as EngineTypes>::ExecutionPayloadEnvelopeV4> {
        debug!(payload_id = %payload_id, timestamp = %chrono::Utc::now(), "Fetching payload");
        Ok(BaseEngineApiClient::<BaseEngineTypes>::get_payload_v4(&self.client().await, payload_id)
            .await?)
    }

    /// Submits a new execution payload for validation.
    pub async fn new_payload(
        &self,
        payload: BaseExecutionPayloadV4,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Requests,
    ) -> eyre::Result<PayloadStatus> {
        debug!(timestamp = %chrono::Utc::now(), "Submitting new payload");
        Ok(BaseEngineApiClient::<BaseEngineTypes>::new_payload_v4(
            &self.client().await,
            payload,
            versioned_hashes,
            parent_beacon_block_root,
            execution_requests,
        )
        .await?)
    }

    /// Sends a forkchoice update, optionally triggering a new payload build.
    pub async fn update_forkchoice(
        &self,
        current_head: B256,
        new_head: B256,
        payload_attributes: Option<<BaseEngineTypes as PayloadTypes>::PayloadAttributes>,
    ) -> eyre::Result<ForkchoiceUpdated> {
        debug!(timestamp = %chrono::Utc::now(), "Updating forkchoice");
        Ok(BaseEngineApiClient::<BaseEngineTypes>::fork_choice_updated_v3(
            &self.client().await,
            ForkchoiceState {
                head_block_hash: new_head,
                safe_block_hash: current_head,
                finalized_block_hash: current_head,
            },
            payload_attributes,
        )
        .await?)
    }
}

/// JSON-RPC interface for block queries used in tests.
#[rpc(server, client, namespace = "eth")]
pub trait BlockApi {
    /// Returns a block by number or tag, optionally including full transactions.
    #[method(name = "getBlockByNumber")]
    async fn get_block_by_number(
        &self,
        block_number: BlockNumberOrTag,
        include_txs: bool,
    ) -> RpcResult<Option<alloy_rpc_types_eth::Block>>;
}

/// Generates a genesis JSON file from the embedded template, stamped with the current time.
pub fn generate_genesis(output: Option<String>) -> eyre::Result<()> {
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
        info!(output = %output, "Generated genesis file at");
    } else {
        println!("{}", serde_json::to_string_pretty(&genesis)?);
    }

    Ok(())
}
