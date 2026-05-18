use std::fmt::Debug;

use async_trait::async_trait;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_rpc::ConductorApiClient;
use jsonrpsee::{
    core::ClientError,
    http_client::{HttpClient, HttpClientBuilder},
};
use url::Url;

/// Trait for interacting with the conductor service.
///
/// The conductor service is responsible for coordinating sequencer behavior
/// in a high-availability setup with leader election.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait Conductor: Debug + Send + Sync {
    /// Check if this node is the conductor leader.
    async fn leader(&self) -> Result<bool, ConductorError>;

    /// Check if the conductor is active.
    async fn active(&self) -> Result<bool, ConductorError>;

    /// Commit an unsafe payload to the conductor.
    async fn commit_unsafe_payload(
        &self,
        payload: &BaseExecutionPayloadEnvelope,
    ) -> Result<(), ConductorError>;

    /// Override the leader of the conductor.
    async fn override_leader(&self) -> Result<(), ConductorError>;
}

/// A client for communicating with the conductor service via RPC.
#[derive(Debug, Clone)]
pub struct ConductorClient {
    /// The inner HTTP client.
    inner: HttpClient,
}

#[async_trait]
impl Conductor for ConductorClient {
    async fn leader(&self) -> Result<bool, ConductorError> {
        Ok(self.inner.conductor_leader().await?)
    }

    async fn active(&self) -> Result<bool, ConductorError> {
        Ok(self.inner.conductor_active().await?)
    }

    async fn commit_unsafe_payload(
        &self,
        payload: &BaseExecutionPayloadEnvelope,
    ) -> Result<(), ConductorError> {
        Ok(self.inner.conductor_commit_unsafe_payload(payload.clone()).await?)
    }

    async fn override_leader(&self) -> Result<(), ConductorError> {
        Ok(self.inner.conductor_override_leader().await?)
    }
}

impl ConductorClient {
    /// Creates a new conductor client using HTTP transport.
    pub fn new_http(url: Url) -> Result<Self, ConductorError> {
        let inner = HttpClientBuilder::default().build(url)?;
        Ok(Self { inner })
    }
}

/// Error type for conductor operations.
#[derive(Debug, thiserror::Error)]
pub enum ConductorError {
    /// An error occurred while making an RPC call to the conductor.
    #[error("RPC error: {0}")]
    Rpc(#[from] ClientError),
    /// The conductor rejected the payload because this node is not the leader.
    #[error("not the conductor leader")]
    NotLeader,
}
