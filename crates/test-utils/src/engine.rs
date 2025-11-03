//! Engine API integration for canonical block production
//!
//! This module provides a typed, type-safe Engine API client based on
//! reth's OpEngineApiClient trait instead of raw string-based RPC calls.

use alloy_eips::eip7685::Requests;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceUpdated, PayloadId, PayloadStatus};
use eyre::Result;
use jsonrpsee::core::client::SubscriptionClientT;
use op_alloy_rpc_types_engine::OpExecutionPayloadV4;
use reth::api::{EngineTypes, PayloadTypes};
use reth::rpc::types::engine::ForkchoiceState;
use reth_optimism_node::OpEngineTypes;
use reth_optimism_rpc::engine::OpEngineApiClient;
use reth_rpc_layer::{AuthClientLayer, JwtSecret};
use reth_tracing::tracing::debug;
use std::marker::PhantomData;
use std::time::Duration;
use url::Url;

/// Default JWT secret for testing
const DEFAULT_JWT_SECRET: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone, Debug)]
pub enum EngineAddress {
    Http(Url),
    Ipc(String),
}

pub trait EngineProtocol: Send + Sync {
    fn client(
        jwt: JwtSecret,
        address: EngineAddress,
    ) -> impl std::future::Future<
        Output = impl jsonrpsee::core::client::SubscriptionClientT + Send + Sync + Unpin + 'static,
    > + Send;
}

pub struct HttpEngine;

impl EngineProtocol for HttpEngine {
    async fn client(
        jwt: JwtSecret,
        address: EngineAddress,
    ) -> impl SubscriptionClientT + Send + Sync + Unpin + 'static {
        let EngineAddress::Http(url) = address else {
            unreachable!();
        };

        let secret_layer = AuthClientLayer::new(jwt);
        let middleware = tower::ServiceBuilder::default().layer(secret_layer);

        jsonrpsee::http_client::HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(10))
            .set_http_middleware(middleware)
            .build(url)
            .expect("Failed to create http client")
    }
}

pub struct IpcEngine;

impl EngineProtocol for IpcEngine {
    async fn client(
        _: JwtSecret, // ipc does not use JWT
        address: EngineAddress,
    ) -> impl SubscriptionClientT + Send + Sync + Unpin + 'static {
        let EngineAddress::Ipc(path) = address else {
            unreachable!();
        };
        reth_ipc::client::IpcClientBuilder::default()
            .build(&path)
            .await
            .expect("Failed to create ipc client")
    }
}

pub struct EngineApi<P: EngineProtocol = HttpEngine> {
    address: EngineAddress,
    jwt_secret: JwtSecret,
    _phantom: PhantomData<P>,
}

impl EngineApi<HttpEngine> {
    pub fn new(engine_url: String) -> Result<Self> {
        let url: Url = engine_url.parse()?;
        let jwt_secret: JwtSecret = DEFAULT_JWT_SECRET.parse()?;

        Ok(Self {
            address: EngineAddress::Http(url),
            jwt_secret,
            _phantom: PhantomData,
        })
    }
}

impl EngineApi<IpcEngine> {
    pub fn new(path: String) -> Result<Self> {
        let jwt_secret: JwtSecret = DEFAULT_JWT_SECRET.parse()?;

        Ok(Self {
            address: EngineAddress::Ipc(path),
            jwt_secret,
            _phantom: PhantomData,
        })
    }
}

impl<P: EngineProtocol> EngineApi<P> {
    /// Get a client instance
    async fn client(&self) -> impl SubscriptionClientT + Send + Sync + Unpin + 'static + use<P> {
        P::client(self.jwt_secret, self.address.clone()).await
    }

    /// Get a payload by ID from the Engine API
    pub async fn get_payload(
        &self,
        payload_id: PayloadId,
    ) -> eyre::Result<<OpEngineTypes as EngineTypes>::ExecutionPayloadEnvelopeV4> {
        debug!(
            "Fetching payload with id: {} at {}",
            payload_id,
            chrono::Utc::now()
        );
        Ok(
            OpEngineApiClient::<OpEngineTypes>::get_payload_v4(&self.client().await, payload_id)
                .await?,
        )
    }

    /// Submit a new payload to the Engine API
    pub async fn new_payload(
        &self,
        payload: OpExecutionPayloadV4,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Requests,
    ) -> eyre::Result<PayloadStatus> {
        debug!("Submitting new payload at {}...", chrono::Utc::now());
        Ok(OpEngineApiClient::<OpEngineTypes>::new_payload_v4(
            &self.client().await,
            payload,
            versioned_hashes,
            parent_beacon_block_root,
            execution_requests,
        )
        .await?)
    }

    /// Update forkchoice on the Engine API
    pub async fn update_forkchoice(
        &self,
        current_head: B256,
        new_head: B256,
        payload_attributes: Option<<OpEngineTypes as PayloadTypes>::PayloadAttributes>,
    ) -> eyre::Result<ForkchoiceUpdated> {
        debug!(
            "Updating forkchoice at {} (current: {}, new: {})",
            chrono::Utc::now(),
            current_head,
            new_head
        );
        let result = OpEngineApiClient::<OpEngineTypes>::fork_choice_updated_v3(
            &self.client().await,
            ForkchoiceState {
                head_block_hash: new_head,
                safe_block_hash: current_head,
                finalized_block_hash: current_head,
            },
            payload_attributes,
        )
        .await;

        match &result {
            Ok(fcu) => debug!("Forkchoice updated successfully: {:?}", fcu),
            Err(e) => debug!("Forkchoice update failed: {:?}", e),
        }

        Ok(result?)
    }
}
