//! HTTP API for execution-layer P2P reachability checks: request validation
//! and probe concurrency limits.

use std::{net::SocketAddr, str::FromStr, sync::Arc};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use reth_network_peers::{NodeRecord, id2pk};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tracing::{debug, info};

use crate::prober::{ReachabilityProber, RlpxProbeOutcome, RlpxProbeStage, RlpxProbeTarget};

/// HTTP path for execution-layer P2P reachability checks.
pub const P2P_REACHABILITY_PATH: &str = "/v1/p2p/reachability/el";
/// Maximum JSON request body size accepted by the reachability route.
pub const P2P_REACHABILITY_MAX_REQUEST_BYTES: usize = 1024;
/// Maximum number of reachability probes allowed in flight globally.
pub const P2P_REACHABILITY_MAX_CONCURRENT_PROBES: usize = 32;
/// Valid execution-layer node identity shared by reachability tests.
#[cfg(test)]
pub const TEST_NODE_ID: &str = "2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4";

/// JSON request for an execution-layer reachability check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct P2pReachabilityRequest {
    /// The node's advertised `enode://` URL, as printed on startup and
    /// returned by `admin_nodeInfo`.
    pub enode: String,
}

impl P2pReachabilityRequest {
    /// Validates the request and returns its advertised `RLPx` target.
    pub fn target(&self) -> Option<RlpxProbeTarget> {
        if !self.enode.starts_with("enode://") {
            return None;
        }
        let record = NodeRecord::from_str(&self.enode).ok()?;
        id2pk(record.id).ok()?;
        let address = record.tcp_addr();
        (address.port() != 0).then_some(RlpxProbeTarget { address, node_id: record.id })
    }
}

/// JSON response for a completed execution-layer reachability check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct P2pReachabilityResponse {
    /// Stable outcome of the probe.
    pub outcome: RlpxProbeOutcome,
    /// Protocol stage reached by the probe.
    pub stage: RlpxProbeStage,
    /// Advertised address probed by the service.
    pub observed_address: SocketAddr,
    /// Total probe duration in milliseconds.
    pub elapsed_ms: u64,
    /// Client version returned by the remote devp2p Hello.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}

/// JSON error returned before a reachability probe starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct P2pErrorResponse {
    /// Stable error code.
    pub error: P2pApiError,
}

/// HTTP error returned before a reachability probe starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum P2pApiError {
    /// The JSON body, node identity, address, or port was invalid.
    InvalidRequest,
    /// The JSON body exceeded the route limit.
    PayloadTooLarge,
    /// Probe capacity was exhausted.
    Saturated,
}

impl P2pApiError {
    /// Maps an Axum JSON rejection to the stable reachability API error surface.
    pub fn from_json_rejection(rejection: JsonRejection) -> Self {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            Self::PayloadTooLarge
        } else {
            Self::InvalidRequest
        }
    }

    /// Returns the HTTP status for this error.
    pub const fn status(&self) -> StatusCode {
        match self {
            Self::InvalidRequest => StatusCode::BAD_REQUEST,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Saturated => StatusCode::TOO_MANY_REQUESTS,
        }
    }
}

impl IntoResponse for P2pApiError {
    fn into_response(self) -> Response {
        (self.status(), Json(P2pErrorResponse { error: self })).into_response()
    }
}

/// State shared by execution-layer reachability handlers.
#[derive(Debug, Clone)]
pub struct P2pState {
    limiter: Arc<Semaphore>,
    prober: Arc<dyn ReachabilityProber>,
}

impl P2pState {
    /// Creates handler state with the supplied global probe capacity.
    pub fn new<P>(global_capacity: usize, prober: Arc<P>) -> Self
    where
        P: ReachabilityProber + 'static,
    {
        Self { limiter: Arc::new(Semaphore::new(global_capacity)), prober }
    }
}

/// Axum routes for execution-layer P2P reachability checks.
#[derive(Debug, Clone, Copy, Default)]
pub struct P2pRoutes;

impl P2pRoutes {
    /// Returns a reachability router using an injected prober.
    pub fn router_with_prober<P>(global_capacity: usize, prober: Arc<P>) -> Router
    where
        P: ReachabilityProber + 'static,
    {
        Router::new()
            .route(P2P_REACHABILITY_PATH, post(Self::check))
            .layer(DefaultBodyLimit::max(P2P_REACHABILITY_MAX_REQUEST_BYTES))
            .with_state(P2pState::new(global_capacity, prober))
    }

    /// Handles one execution-layer P2P reachability check.
    pub async fn check(
        State(state): State<P2pState>,
        body: Result<Json<P2pReachabilityRequest>, JsonRejection>,
    ) -> Result<Json<P2pReachabilityResponse>, P2pApiError> {
        let Json(request) = body.map_err(|rejection| {
            debug!(status = %rejection.status(), "reachability request body rejected");
            P2pApiError::from_json_rejection(rejection)
        })?;
        let target = request.target().ok_or_else(|| {
            debug!("reachability request target validation failed");
            P2pApiError::InvalidRequest
        })?;
        let _permit = Arc::clone(&state.limiter).try_acquire_owned().map_err(|_| {
            debug!("reachability probe capacity exhausted");
            P2pApiError::Saturated
        })?;
        let result = state.prober.probe(target).await;
        let elapsed_ms = u64::try_from(result.elapsed.as_millis()).unwrap_or(u64::MAX);

        info!(
            outcome = %result.outcome,
            stage = %result.stage,
            target = %target.address,
            elapsed_ms,
            "reachability probe completed"
        );

        Ok(Json(P2pReachabilityResponse {
            outcome: result.outcome,
            stage: result.stage,
            observed_address: target.address,
            elapsed_ms,
            client_version: result.client_version,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        str::FromStr,
        sync::{Arc, Mutex, PoisonError},
        time::Duration,
    };

    use alloy_primitives::B512;
    use async_trait::async_trait;
    use axum::{Router, http::StatusCode};
    use tokio::{net::TcpListener, sync::Semaphore, task::JoinHandle};

    use super::{
        P2P_REACHABILITY_MAX_CONCURRENT_PROBES, P2P_REACHABILITY_PATH, P2pReachabilityRequest,
        P2pReachabilityResponse, P2pRoutes, TEST_NODE_ID,
    };
    use crate::{
        ReachabilityProber, RlpxProbeOutcome, RlpxProbeResult, RlpxProbeStage, RlpxProbeTarget,
    };

    #[derive(Debug, Clone, Default)]
    struct FakeProber {
        targets: Arc<Mutex<Vec<RlpxProbeTarget>>>,
    }

    #[async_trait]
    impl ReachabilityProber for FakeProber {
        async fn probe(&self, target: RlpxProbeTarget) -> RlpxProbeResult {
            self.targets.lock().unwrap_or_else(PoisonError::into_inner).push(target);
            RlpxProbeResult {
                outcome: RlpxProbeOutcome::Reachable,
                stage: RlpxProbeStage::Rlpx,
                elapsed: Duration::from_millis(12),
                client_version: Some("test-peer/1.0".to_string()),
            }
        }
    }

    #[derive(Debug, Clone)]
    struct BlockingProber {
        entered: Arc<Semaphore>,
        release: Arc<Semaphore>,
    }

    #[async_trait]
    impl ReachabilityProber for BlockingProber {
        async fn probe(&self, _: RlpxProbeTarget) -> RlpxProbeResult {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            RlpxProbeResult {
                outcome: RlpxProbeOutcome::Reachable,
                stage: RlpxProbeStage::Rlpx,
                elapsed: Duration::from_millis(1),
                client_version: None,
            }
        }
    }

    async fn start_test_server(router: Router) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (address, handle)
    }

    fn test_request() -> P2pReachabilityRequest {
        P2pReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@8.8.8.8:30303") }
    }

    #[test]
    fn validates_enode_identity_address_and_port() {
        assert_eq!(
            test_request().target().unwrap().address,
            SocketAddr::from(([8, 8, 8, 8], 30303))
        );

        let with_discport = P2pReachabilityRequest {
            enode: format!("enode://{TEST_NODE_ID}@8.8.8.8:30303?discport=30301"),
        };
        assert_eq!(
            with_discport.target().unwrap().address,
            SocketAddr::from(([8, 8, 8, 8], 30303))
        );

        let invalid_discport = P2pReachabilityRequest {
            enode: format!("enode://{TEST_NODE_ID}@8.8.8.8:30303?discport=invalid"),
        };
        assert!(invalid_discport.target().is_none());

        let ipv6 = P2pReachabilityRequest {
            enode: format!("enode://{TEST_NODE_ID}@[2606:4700:4700::1111]:30303"),
        };
        assert_eq!(ipv6.target().unwrap().address, "[2606:4700:4700::1111]:30303".parse().unwrap());

        let zero_port =
            P2pReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@8.8.8.8:0") };
        assert!(zero_port.target().is_none());

        let invalid_identity =
            P2pReachabilityRequest { enode: format!("enode://{}@8.8.8.8:30303", "00".repeat(64)) };
        assert!(invalid_identity.target().is_none());

        let missing_scheme =
            P2pReachabilityRequest { enode: format!("{TEST_NODE_ID}@8.8.8.8:30303") };
        assert!(missing_scheme.target().is_none());

        let hostname =
            P2pReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@example.com:30303") };
        assert!(hostname.target().is_none());

        let private =
            P2pReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@10.0.0.1:30303") };
        assert_eq!(private.target().unwrap().address, SocketAddr::from(([10, 0, 0, 1], 30303)));
    }

    #[test]
    fn probes_advertised_enode_address() {
        let request =
            P2pReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@9.9.9.9:30303") };

        let target = request.target().unwrap();

        assert_eq!(target.address, SocketAddr::from(([9, 9, 9, 9], 30303)));
    }

    #[tokio::test]
    async fn ignores_forwarded_header_and_probes_enode_address() {
        let prober = Arc::new(FakeProber::default());
        let router = P2pRoutes::router_with_prober(
            P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            Arc::clone(&prober),
        );
        let (address, handle) = start_test_server(router).await;
        let request = test_request();

        let response = reqwest::Client::new()
            .post(format!("http://{address}{P2P_REACHABILITY_PATH}"))
            .header("x-forwarded-for", "1.1.1.1")
            .json(&request)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let response = response.json::<P2pReachabilityResponse>().await.unwrap();
        assert_eq!(response.outcome, RlpxProbeOutcome::Reachable);
        assert_eq!(response.observed_address, SocketAddr::from(([8, 8, 8, 8], 30303)));
        assert_eq!(response.client_version.as_deref(), Some("test-peer/1.0"));
        assert_eq!(
            prober.targets.lock().unwrap_or_else(PoisonError::into_inner).as_slice(),
            &[RlpxProbeTarget {
                address: SocketAddr::from(([8, 8, 8, 8], 30303)),
                node_id: B512::from_str(TEST_NODE_ID).unwrap(),
            }]
        );

        handle.abort();
    }

    #[tokio::test]
    async fn probes_private_target() {
        let prober = Arc::new(FakeProber::default());
        let router = P2pRoutes::router_with_prober(
            P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            Arc::clone(&prober),
        );
        let (address, handle) = start_test_server(router).await;
        let request =
            P2pReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@10.0.0.1:30303") };

        let response = reqwest::Client::new()
            .post(format!("http://{address}{P2P_REACHABILITY_PATH}"))
            .json(&request)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            prober.targets.lock().unwrap_or_else(PoisonError::into_inner)[0].address,
            SocketAddr::from(([10, 0, 0, 1], 30303))
        );
        handle.abort();
    }

    #[tokio::test]
    async fn rejects_oversized_body() {
        let router = P2pRoutes::router_with_prober(
            P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            Arc::new(FakeProber::default()),
        );
        let (address, handle) = start_test_server(router).await;

        let response = reqwest::Client::new()
            .post(format!("http://{address}{P2P_REACHABILITY_PATH}"))
            .header("content-type", "application/json")
            .body("x".repeat(2048))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        handle.abort();
    }

    #[tokio::test]
    async fn rejects_probe_when_global_capacity_is_exhausted() {
        let prober = Arc::new(BlockingProber {
            entered: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
        });
        let router = P2pRoutes::router_with_prober(1, Arc::clone(&prober));
        let (address, handle) = start_test_server(router).await;
        let request = test_request();
        let client = reqwest::Client::new();
        let first_client = client.clone();
        let first_request = request.clone();

        let first = tokio::spawn(async move {
            first_client
                .post(format!("http://{address}{P2P_REACHABILITY_PATH}"))
                .json(&first_request)
                .send()
                .await
                .unwrap()
        });
        prober.entered.acquire().await.unwrap().forget();

        let second = client
            .post(format!("http://{address}{P2P_REACHABILITY_PATH}"))
            .json(&request)
            .send()
            .await
            .unwrap();

        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        prober.release.add_permits(1);
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
        handle.abort();
    }
}
