//! HTTP API for node telemetry report ingest.

use std::{net::SocketAddr, num::NonZeroU32, sync::Arc};

use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use base_telemetry_types::{NodeReport, NodeReportEvent};
use base_trusted_proxy::TrustedProxyConfig;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::ReportRecorder;

/// HTTP path that accepts node report submissions.
pub const NODE_REPORT_PATH: &str = "/v1/ingest";
/// Maximum JSON request body size accepted by the ingest route.
///
/// A full report is around a kilobyte. The limit leaves room for the latency sample array and
/// for fields added by later schema versions, and nothing else.
pub const NODE_REPORT_MAX_REQUEST_BYTES: usize = 16 * 1024;
/// Default node reports accepted per client IP per hour.
///
/// Nodes report every fifteen minutes, so four an hour is the honest rate. Sixty leaves room for
/// a restart loop and for many nodes sharing one NAT without ever throttling honest traffic.
pub const DEFAULT_NODE_REPORT_REQUESTS_PER_HOUR: NonZeroU32 = NonZeroU32::new(60).unwrap();

/// JSON error returned when a node report is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestErrorResponse {
    /// Stable error code.
    pub error: IngestApiError,
}

/// HTTP error returned when a node report is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestApiError {
    /// The JSON body was malformed or did not match the report schema.
    InvalidRequest,
    /// The JSON body exceeded the route limit.
    PayloadTooLarge,
}

impl IngestApiError {
    /// Maps an Axum JSON rejection to the stable ingest API error surface.
    pub fn from_json_rejection(rejection: &JsonRejection) -> Self {
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
        }
    }
}

impl IntoResponse for IngestApiError {
    fn into_response(self) -> Response {
        (self.status(), Json(IngestErrorResponse { error: self })).into_response()
    }
}

/// State shared by the ingest handler.
#[derive(Debug, Clone)]
pub struct IngestState {
    recorder: Arc<dyn ReportRecorder>,
    proxy: Arc<TrustedProxyConfig>,
}

impl IngestState {
    /// Creates handler state from a recorder and the trusted proxy configuration.
    pub const fn new(recorder: Arc<dyn ReportRecorder>, proxy: Arc<TrustedProxyConfig>) -> Self {
        Self { recorder, proxy }
    }
}

/// Axum routes for node telemetry ingest.
#[derive(Debug, Clone, Copy, Default)]
pub struct IngestRoutes;

impl IngestRoutes {
    /// Returns an ingest router writing to the supplied recorder.
    pub fn router(recorder: Arc<dyn ReportRecorder>, proxy: Arc<TrustedProxyConfig>) -> Router {
        Router::new()
            .route(NODE_REPORT_PATH, post(Self::ingest_node_report))
            .layer(DefaultBodyLimit::max(NODE_REPORT_MAX_REQUEST_BYTES))
            .with_state(IngestState::new(recorder, proxy))
    }

    /// Accepts one node report.
    ///
    /// Returns 202 with an empty body: recording is asynchronous, and a node must never wait on
    /// our storage to finish its reporting cycle.
    pub async fn ingest_node_report(
        State(state): State<IngestState>,
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
        headers: HeaderMap,
        body: Result<Json<NodeReport>, JsonRejection>,
    ) -> Result<StatusCode, IngestApiError> {
        let Json(report) = body.map_err(|rejection| {
            debug!(status = %rejection.status(), "node report body rejected");
            IngestApiError::from_json_rejection(&rejection)
        })?;

        if !report.is_current_schema() {
            // Accepted anyway: an old or new node must keep reporting across a schema change,
            // and every field that did parse is still worth having.
            warn!(
                schema_version = %report.schema_version,
                telemetry_id = %report.telemetry_id,
                "node report declares an unrecognized schema version"
            );
        }

        let observed_ip = state.proxy.client_ip(peer.ip(), &headers);
        let event = NodeReportEvent::new(report, Utc::now(), observed_ip);
        state.recorder.record(&event);

        Ok(StatusCode::ACCEPTED)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{Arc, Mutex},
    };

    use axum::{Router, http::StatusCode};
    use base_telemetry_types::{
        ClientMeta, Heads, IpSource, NODE_REPORT_SCHEMA_VERSION, NetHealth, NodeReportEvent,
    };
    use base_trusted_proxy::TrustedProxyConfig;
    use tokio::{net::TcpListener, task::JoinHandle};
    use uuid::Uuid;

    use super::*;
    use crate::CLIENT_IP_HEADER;

    /// Captures recorded events in memory.
    ///
    /// Hand-rolled rather than `automock`ed because the assertions are about the events that
    /// accumulate across several requests, not about call counts on a single one.
    #[derive(Debug, Default)]
    struct CapturingRecorder {
        events: Mutex<Vec<NodeReportEvent>>,
    }

    impl CapturingRecorder {
        fn events(&self) -> Vec<NodeReportEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl ReportRecorder for CapturingRecorder {
        fn record(&self, event: &NodeReportEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

    fn sample_report() -> NodeReport {
        NodeReport {
            telemetry_id: Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            client: ClientMeta {
                client_version: "1.2.3".to_string(),
                git_sha: "abc1234".to_string(),
                l2_chain_id: 8453,
                network: "mainnet".to_string(),
                uptime_secs: 600,
                ..Default::default()
            },
            heads: Heads { unsafe_block: 42, safe_block: Some(40), ..Default::default() },
            net_health: NetHealth { peer_count: 17, ..Default::default() },
            ..Default::default()
        }
    }

    async fn start_test_server(router: Router) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .unwrap();
        });
        (address, handle)
    }

    fn router_with(recorder: Arc<CapturingRecorder>, trusted_cidrs: Vec<ipnet::IpNet>) -> Router {
        IngestRoutes::router(
            recorder,
            Arc::new(TrustedProxyConfig::new(CLIENT_IP_HEADER.to_string(), trusted_cidrs)),
        )
    }

    #[tokio::test]
    async fn test_valid_report_is_accepted_with_an_empty_body() {
        let recorder = Arc::new(CapturingRecorder::default());
        let (address, handle) =
            start_test_server(router_with(Arc::clone(&recorder), Vec::new())).await;

        let response = reqwest::Client::new()
            .post(format!("http://{address}{NODE_REPORT_PATH}"))
            .json(&sample_report())
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(response.bytes().await.unwrap().is_empty());

        let events = recorder.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].report.heads.unsafe_block, 42);
        assert_eq!(events[0].report.net_health.peer_count, 17);
        assert_eq!(
            events[0].ip_source,
            IpSource::ServerObserved,
            "a report without an advertised IP is attributed to the connection"
        );
        assert_eq!(events[0].reported_ip, IpAddr::V4(Ipv4Addr::LOCALHOST));

        handle.abort();
    }

    #[tokio::test]
    async fn test_advertised_ip_wins_over_the_observed_one() {
        let recorder = Arc::new(CapturingRecorder::default());
        let (address, handle) =
            start_test_server(router_with(Arc::clone(&recorder), Vec::new())).await;
        let advertised = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
        let report = NodeReport {
            net_health: NetHealth { advertised_ip: Some(advertised), ..Default::default() },
            ..sample_report()
        };

        reqwest::Client::new()
            .post(format!("http://{address}{NODE_REPORT_PATH}"))
            .json(&report)
            .send()
            .await
            .unwrap();

        let events = recorder.events();
        assert_eq!(events[0].reported_ip, advertised);
        assert_eq!(events[0].ip_source, IpSource::NodeProvided);

        handle.abort();
    }

    #[tokio::test]
    async fn test_forwarded_ip_is_used_only_from_a_trusted_proxy() {
        let recorder = Arc::new(CapturingRecorder::default());
        let trusted = vec!["127.0.0.0/8".parse().unwrap()];
        let (address, handle) =
            start_test_server(router_with(Arc::clone(&recorder), trusted)).await;

        reqwest::Client::new()
            .post(format!("http://{address}{NODE_REPORT_PATH}"))
            .header(CLIENT_IP_HEADER, "198.51.100.4")
            .json(&sample_report())
            .send()
            .await
            .unwrap();

        let events = recorder.events();
        assert_eq!(events[0].reported_ip, IpAddr::V4(Ipv4Addr::new(198, 51, 100, 4)));

        handle.abort();
    }

    #[tokio::test]
    async fn test_malformed_json_is_rejected() {
        let recorder = Arc::new(CapturingRecorder::default());
        let (address, handle) =
            start_test_server(router_with(Arc::clone(&recorder), Vec::new())).await;

        let response = reqwest::Client::new()
            .post(format!("http://{address}{NODE_REPORT_PATH}"))
            .header("content-type", "application/json")
            .body("{\"schema_version\":")
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: IngestErrorResponse = response.json().await.unwrap();
        assert_eq!(body.error, IngestApiError::InvalidRequest);
        assert!(recorder.events().is_empty());

        handle.abort();
    }

    #[tokio::test]
    async fn test_oversized_body_is_rejected() {
        let recorder = Arc::new(CapturingRecorder::default());
        let (address, handle) =
            start_test_server(router_with(Arc::clone(&recorder), Vec::new())).await;
        let report = NodeReport {
            config: base_telemetry_types::NodeConfigReport {
                experimental_flags: vec!["x".repeat(1024); 32],
                ..Default::default()
            },
            ..sample_report()
        };

        let response = reqwest::Client::new()
            .post(format!("http://{address}{NODE_REPORT_PATH}"))
            .json(&report)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body: IngestErrorResponse = response.json().await.unwrap();
        assert_eq!(body.error, IngestApiError::PayloadTooLarge);
        assert!(recorder.events().is_empty());

        handle.abort();
    }

    #[tokio::test]
    async fn test_unrecognized_schema_version_is_still_recorded() {
        let recorder = Arc::new(CapturingRecorder::default());
        let (address, handle) =
            start_test_server(router_with(Arc::clone(&recorder), Vec::new())).await;
        let report =
            NodeReport { schema_version: NODE_REPORT_SCHEMA_VERSION + 1, ..sample_report() };

        let response = reqwest::Client::new()
            .post(format!("http://{address}{NODE_REPORT_PATH}"))
            .json(&report)
            .send()
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "a node must keep reporting across a schema change"
        );
        assert_eq!(recorder.events().len(), 1);

        handle.abort();
    }
}
