//! HTTP client and wire types for Base telemetry reachability checks.

use std::{net::SocketAddr, time::Duration};

use alloy_transport_http::reqwest::{self, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use url::Url;

const EL_REACHABILITY_PATH: &str = "/v1/p2p/reachability/el";
const CL_REACHABILITY_PATH: &str = "/v1/p2p/reachability/cl";
const TELEMETRY_REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

/// JSON response for a completed reachability check on either layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReachabilityResponse {
    /// Stable outcome of the probe.
    pub outcome: ReachabilityOutcome,
    /// Stable label of the protocol stage reached by the probe, such as
    /// `tcp_connect`. Kept as a string because the client only displays it
    /// and the per-layer stage sets can grow independently.
    pub stage: String,
    /// Advertised address probed by the telemetry service.
    pub observed_address: SocketAddr,
    /// Total probe duration in milliseconds.
    pub elapsed_ms: u64,
    /// Client version returned by the remote handshake, when advertised.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}

/// Stable outcome returned by a reachability probe on either layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReachabilityOutcome {
    /// TCP and the layer's security handshake reached the remote node.
    Reachable,
    /// The telemetry service could not establish the target TCP connection.
    ConnectionFailed,
    /// The probe deadline elapsed.
    TimedOut,
    /// TCP connected, but the security handshake failed.
    HandshakeFailed,
}

impl ReachabilityOutcome {
    /// Returns the stable wire label for this outcome.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Reachable => "reachable",
            Self::ConnectionFailed => "connection_failed",
            Self::TimedOut => "timed_out",
            Self::HandshakeFailed => "handshake_failed",
        }
    }
}

/// JSON error returned before a telemetry reachability probe starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryErrorResponse {
    /// Stable API error code.
    pub error: TelemetryApiError,
}

/// Stable error returned by the telemetry reachability API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryApiError {
    /// The JSON body, node identity, address, or port was invalid.
    InvalidRequest,
    /// The JSON body exceeded the route limit.
    PayloadTooLarge,
    /// Probe capacity was exhausted.
    Saturated,
    /// The client IP exceeded the request rate limit.
    RateLimited,
}

/// Error returned by the Base telemetry HTTP client.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum TelemetryClientError {
    /// The telemetry service rejected the supplied reachability target.
    #[error("telemetry service rejected the reachability request as invalid")]
    InvalidRequest,
    /// The telemetry service rejected the request body as too large.
    #[error("telemetry service rejected the reachability request as too large")]
    PayloadTooLarge,
    /// The telemetry service had no reachability probe capacity available.
    #[error("telemetry service reachability probe capacity is saturated")]
    Saturated,
    /// The telemetry service rate limited the client IP.
    #[error("telemetry service rate limited the reachability request")]
    RateLimited,
    /// The telemetry service could not be reached or returned an unusable response.
    #[error("telemetry service unavailable: {message}")]
    Unavailable {
        /// Underlying transport, URL, status, or decoding error.
        message: String,
    },
}

impl From<reqwest::Error> for TelemetryClientError {
    fn from(error: reqwest::Error) -> Self {
        Self::Unavailable { message: error.to_string() }
    }
}

/// HTTP client for Base telemetry APIs.
#[derive(Debug, Clone)]
pub struct TelemetryClient {
    base_url: Url,
    http: reqwest::Client,
}

impl TelemetryClient {
    /// Returns the hosted telemetry backend base URL for a supported Base chain.
    pub fn backend_base_url(chain_id: u64) -> Option<Url> {
        Url::parse(match chain_id {
            8453 => "https://mainnet.telemetry.base.org",
            84532 => "https://sepolia.telemetry.base.org",
            _ => return None,
        })
        .ok()
    }

    /// Creates a telemetry client with a timeout longer than the backend probe deadline.
    pub fn new(base_url: Url) -> Result<Self, TelemetryClientError> {
        let http = reqwest::Client::builder().timeout(TELEMETRY_REQUEST_TIMEOUT).build()?;
        Ok(Self { base_url, http })
    }

    /// Requests an external execution-layer reachability check for `enode`.
    pub async fn check_el_reachability(
        &self,
        enode: &str,
    ) -> Result<ReachabilityResponse, TelemetryClientError> {
        self.check_reachability(EL_REACHABILITY_PATH, &json!({ "enode": enode })).await
    }

    /// Requests an external consensus-layer reachability check for an `enr:`
    /// record or a public `IPv4` `/ip4/.../tcp/.../p2p/<peer-id>` multiaddr.
    pub async fn check_cl_reachability(
        &self,
        enr: &str,
    ) -> Result<ReachabilityResponse, TelemetryClientError> {
        self.check_reachability(CL_REACHABILITY_PATH, &json!({ "enr": enr })).await
    }

    /// Posts one reachability request and decodes the typed response or error.
    async fn check_reachability(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<ReachabilityResponse, TelemetryClientError> {
        let response = self.http.post(self.reachability_endpoint(path)?).json(body).send().await?;
        let status = response.status();

        if status.is_success() {
            return Ok(response.json().await?);
        }

        let TelemetryErrorResponse { error } =
            response.json().await.map_err(|error| TelemetryClientError::Unavailable {
                message: format!("unexpected HTTP {status} response: {error}"),
            })?;
        match (status, error) {
            (StatusCode::BAD_REQUEST, TelemetryApiError::InvalidRequest) => {
                Err(TelemetryClientError::InvalidRequest)
            }
            (StatusCode::PAYLOAD_TOO_LARGE, TelemetryApiError::PayloadTooLarge) => {
                Err(TelemetryClientError::PayloadTooLarge)
            }
            (StatusCode::TOO_MANY_REQUESTS, TelemetryApiError::Saturated) => {
                Err(TelemetryClientError::Saturated)
            }
            (StatusCode::TOO_MANY_REQUESTS, TelemetryApiError::RateLimited) => {
                Err(TelemetryClientError::RateLimited)
            }
            _ => Err(TelemetryClientError::Unavailable {
                message: format!("unexpected HTTP {status} response with error {error:?}"),
            }),
        }
    }

    /// Returns the reachability endpoint while preserving the configured path prefix.
    fn reachability_endpoint(&self, path: &str) -> Result<Url, TelemetryClientError> {
        let mut endpoint = self.base_url.clone();
        endpoint
            .path_segments_mut()
            .map_err(|()| TelemetryClientError::Unavailable {
                message: format!("telemetry URL `{}` cannot have a path appended", self.base_url),
            })?
            .pop_if_empty()
            .extend(path.split('/').filter(|segment| !segment.is_empty()));
        Ok(endpoint)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::{
        Json, Router,
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::post,
    };
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinHandle};
    use url::Url;

    use super::{
        CL_REACHABILITY_PATH, EL_REACHABILITY_PATH, ReachabilityOutcome, ReachabilityResponse,
        TelemetryApiError, TelemetryClient, TelemetryClientError, TelemetryErrorResponse,
    };

    async fn start_server(router: Router) -> (Url, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (Url::parse(&format!("http://{address}")).unwrap(), handle)
    }

    async fn reachable(Json(request): Json<Value>) -> Json<Value> {
        assert_eq!(request["enode"], "enode://test");
        Json(json!({
            "outcome": "reachable",
            "stage": "devp2p_hello",
            "observedAddress": "203.0.113.10:30303",
            "elapsedMs": 42,
            "clientVersion": "reth/v1.0.0",
        }))
    }

    async fn typed_error(Json(request): Json<Value>) -> Response {
        let (status, error) = match request["enode"].as_str().unwrap() {
            "invalid" => (StatusCode::BAD_REQUEST, TelemetryApiError::InvalidRequest),
            "large" => (StatusCode::PAYLOAD_TOO_LARGE, TelemetryApiError::PayloadTooLarge),
            "saturated" => (StatusCode::TOO_MANY_REQUESTS, TelemetryApiError::Saturated),
            "limited" => (StatusCode::TOO_MANY_REQUESTS, TelemetryApiError::RateLimited),
            _ => panic!("unexpected test request"),
        };
        (status, Json(TelemetryErrorResponse { error })).into_response()
    }

    #[tokio::test]
    async fn decodes_camel_case_fields_and_snake_case_enums() {
        let router = Router::new().route(EL_REACHABILITY_PATH, post(reachable));
        let (base_url, handle) = start_server(router).await;
        let client = TelemetryClient::new(base_url).unwrap();

        let response = client.check_el_reachability("enode://test").await.unwrap();

        assert_eq!(
            response,
            ReachabilityResponse {
                outcome: ReachabilityOutcome::Reachable,
                stage: "devp2p_hello".to_string(),
                observed_address: SocketAddr::from(([203, 0, 113, 10], 30303)),
                elapsed_ms: 42,
                client_version: Some("reth/v1.0.0".to_string()),
            }
        );
        handle.abort();
    }

    #[tokio::test]
    async fn decodes_cl_reachability_response() {
        async fn cl_reachable(Json(request): Json<Value>) -> Json<Value> {
            assert_eq!(request["enr"], "enr:test");
            Json(json!({
                "outcome": "reachable",
                "stage": "identify",
                "observedAddress": "203.0.113.10:9222",
                "elapsedMs": 42,
                "clientVersion": "op-node/v1.0.0",
            }))
        }
        let router = Router::new().route(CL_REACHABILITY_PATH, post(cl_reachable));
        let (base_url, handle) = start_server(router).await;
        let client = TelemetryClient::new(base_url).unwrap();

        let response = client.check_cl_reachability("enr:test").await.unwrap();

        assert_eq!(
            response,
            ReachabilityResponse {
                outcome: ReachabilityOutcome::Reachable,
                stage: "identify".to_string(),
                observed_address: SocketAddr::from(([203, 0, 113, 10], 9222)),
                elapsed_ms: 42,
                client_version: Some("op-node/v1.0.0".to_string()),
            }
        );
        handle.abort();
    }

    #[tokio::test]
    async fn preserves_cl_typed_api_errors() {
        async fn cl_error(Json(request): Json<Value>) -> Response {
            assert_eq!(request["enr"], "enr:invalid");
            (
                StatusCode::BAD_REQUEST,
                Json(TelemetryErrorResponse { error: TelemetryApiError::InvalidRequest }),
            )
                .into_response()
        }
        let router = Router::new().route(CL_REACHABILITY_PATH, post(cl_error));
        let (base_url, handle) = start_server(router).await;
        let client = TelemetryClient::new(base_url).unwrap();

        assert_eq!(
            client.check_cl_reachability("enr:invalid").await.unwrap_err(),
            TelemetryClientError::InvalidRequest
        );
        handle.abort();
    }

    #[tokio::test]
    async fn preserves_base_url_path_prefix() {
        let router = Router::new().route("/telemetry/v1/p2p/reachability/el", post(reachable));
        let (base_url, handle) = start_server(router).await;

        for base in [format!("{base_url}telemetry"), format!("{base_url}telemetry/")] {
            let client = TelemetryClient::new(Url::parse(&base).unwrap()).unwrap();
            let response = client.check_el_reachability("enode://test").await.unwrap();
            assert_eq!(response.outcome, ReachabilityOutcome::Reachable);
        }
        handle.abort();
    }

    #[tokio::test]
    async fn preserves_typed_api_errors() {
        let router = Router::new().route(EL_REACHABILITY_PATH, post(typed_error));
        let (base_url, handle) = start_server(router).await;
        let client = TelemetryClient::new(base_url).unwrap();

        assert_eq!(
            client.check_el_reachability("invalid").await.unwrap_err(),
            TelemetryClientError::InvalidRequest
        );
        assert_eq!(
            client.check_el_reachability("large").await.unwrap_err(),
            TelemetryClientError::PayloadTooLarge
        );
        assert_eq!(
            client.check_el_reachability("saturated").await.unwrap_err(),
            TelemetryClientError::Saturated
        );
        assert_eq!(
            client.check_el_reachability("limited").await.unwrap_err(),
            TelemetryClientError::RateLimited
        );
        handle.abort();
    }

    #[tokio::test]
    async fn reports_transport_failure_as_unavailable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client =
            TelemetryClient::new(Url::parse(&format!("http://{address}")).unwrap()).unwrap();

        let error = client.check_el_reachability("enode://test").await.unwrap_err();

        assert!(matches!(error, TelemetryClientError::Unavailable { .. }));
    }
}
