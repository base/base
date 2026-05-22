use std::{fmt::Debug, time::Duration};

use async_trait::async_trait;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_rpc::ConductorApiClient;
use jsonrpsee::{
    core::ClientError,
    http_client::{HttpClient, HttpClientBuilder},
};
use ssz::Encode;
use url::Url;

/// HTTP route on the conductor that accepts SSZ-encoded payload envelopes.
/// Mirrors `CommitUnsafePayloadPath` in op-conductor.
const COMMIT_UNSAFE_PAYLOAD_PATH: &str = "/commit-unsafe-payload";

/// Content-Type the conductor expects on the binary commit endpoint.
const SSZ_CONTENT_TYPE: &str = "application/octet-stream";

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
///
/// Always uses jsonrpsee for `leader`, `active`, and `override_leader`. For
/// `commit_unsafe_payload`, dispatches to the SSZ-binary endpoint when
/// `binary_commit` is set on construction; otherwise uses the JSON-RPC method.
#[derive(Debug, Clone)]
pub struct ConductorClient {
    /// The inner JSON-RPC HTTP client.
    inner: HttpClient,
    /// The reqwest client + endpoint URL for the binary commit path. `None`
    /// means use JSON-RPC for commits.
    binary: Option<BinaryCommitClient>,
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
        if let Some(bin) = &self.binary {
            return bin.commit_unsafe_payload(payload).await;
        }
        Ok(self.inner.conductor_commit_unsafe_payload(payload.clone()).await?)
    }

    async fn override_leader(&self) -> Result<(), ConductorError> {
        Ok(self.inner.conductor_override_leader().await?)
    }
}

impl ConductorClient {
    /// Creates a new conductor client using HTTP transport (JSON-RPC for all
    /// methods).
    pub fn new_http(url: Url, timeout: Duration) -> Result<Self, ConductorError> {
        let inner = HttpClientBuilder::default().request_timeout(timeout).build(url)?;
        Ok(Self { inner, binary: None })
    }

    /// Creates a new conductor client where `commit_unsafe_payload` uses the
    /// SSZ-binary endpoint at `<url>/commit-unsafe-payload` and the other RPCs
    /// stay on JSON-RPC. The conductor must be running with the binary
    /// endpoint enabled.
    pub fn new_http_with_binary_commit(
        url: Url,
        timeout: Duration,
    ) -> Result<Self, ConductorError> {
        let inner = HttpClientBuilder::default().request_timeout(timeout).build(url.clone())?;
        let binary = BinaryCommitClient::new(url, timeout)?;
        Ok(Self { inner, binary: Some(binary) })
    }
}

/// Thin reqwest wrapper for the conductor's SSZ-binary commit endpoint.
///
/// Wire format (matches op-conductor `BinaryCommitHandler`):
/// ```text
///   POST /commit-unsafe-payload
///   Content-Type: application/octet-stream
///   Body: SSZ-encoded BaseExecutionPayloadEnvelope (raw bytes, no length
///         prefix; for V3+ payloads the parent_beacon_block_root is the first
///         32 bytes per `<BaseExecutionPayloadEnvelope as ssz::Encode>`).
/// ```
/// Returns `Ok(())` on 200, `ConductorError::BinaryRejected` on non-success status codes,
/// or `ConductorError::BinaryRequest` on transport failures.
#[derive(Debug, Clone)]
struct BinaryCommitClient {
    http: reqwest::Client,
    endpoint: Url,
}

impl BinaryCommitClient {
    fn new(base_url: Url, timeout: Duration) -> Result<Self, ConductorError> {
        let endpoint = base_url.join(COMMIT_UNSAFE_PAYLOAD_PATH)?;
        let http = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self { http, endpoint })
    }

    async fn commit_unsafe_payload(
        &self,
        payload: &BaseExecutionPayloadEnvelope,
    ) -> Result<(), ConductorError> {
        let body = payload.as_ssz_bytes();
        let resp = self
            .http
            .post(self.endpoint.clone())
            .header(reqwest::header::CONTENT_TYPE, SSZ_CONTENT_TYPE)
            .body(body)
            .send()
            .await?;
        if resp.status().is_success() {
            return Ok(());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default().trim().to_string();
        Err(ConductorError::BinaryRejected { status, body })
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
    /// A transport-level error on the binary commit endpoint (connection refused, timeout, TLS,
    /// client construction failure, etc.).
    #[error("binary commit request failed")]
    BinaryRequest(#[from] reqwest::Error),
    /// The conductor's binary commit endpoint returned a non-success HTTP status.
    #[error("binary commit rejected: {status}")]
    BinaryRejected {
        /// HTTP status code returned by the conductor.
        status: reqwest::StatusCode,
        /// Response body, typically an error message from the conductor.
        body: String,
    },
    /// The conductor URL could not be parsed into a valid endpoint.
    #[error("invalid conductor url: {0}")]
    InvalidUrl(#[from] url::ParseError),
}
