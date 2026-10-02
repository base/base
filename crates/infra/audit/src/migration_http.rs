//! One listener for cached managed migration probes, status, and Prometheus metrics.

use std::fmt;

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use metrics_exporter_prometheus::PrometheusHandle;

use crate::{MigrationReporter, MigrationStatus};

/// HTTP state; handlers never contend for an active database connection.
#[derive(Clone)]
pub struct MigrationHttp {
    /// Cached migration status.
    pub progress: MigrationReporter,
    /// Recorder-only Prometheus handle, absent when metrics disabled.
    pub metrics: Option<PrometheusHandle>,
}

impl fmt::Debug for MigrationHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MigrationHttp { cached status and recorder }")
    }
}

impl MigrationHttp {
    /// Builds probes and metrics on the supervisor's one listener.
    pub fn router(self) -> Router {
        Router::new()
            .route("/healthz", get(Self::health))
            .route("/readyz", get(Self::ready))
            .route("/status", get(Self::status))
            .route("/metrics", get(Self::metrics))
            .with_state(self)
    }

    /// A terminal operation failure is not a liveness failure.
    pub async fn health() -> &'static str {
        "ok\n"
    }

    /// Availability after schema and worker initialization, independent of completion.
    pub async fn ready(State(state): State<Self>) -> impl IntoResponse {
        if state.progress.snapshot().ready {
            (StatusCode::OK, "ready\n")
        } else {
            (StatusCode::SERVICE_UNAVAILABLE, "not ready\n")
        }
    }

    /// Returns a secret-safe snapshot without backend ownership or state paths.
    pub async fn status(State(state): State<Self>) -> Json<MigrationStatus> {
        Json(state.progress.snapshot())
    }

    /// Renders the existing recorder without installing a competing listener.
    pub async fn metrics(State(state): State<Self>) -> impl IntoResponse {
        (
            [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
            state.metrics.as_ref().map_or_else(String::new, PrometheusHandle::render),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MigrationError, MigrationPhase};
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn cached_http_readiness_is_not_completion_or_operation_success() {
        let progress = MigrationReporter::new("pod-one".into());
        let router = MigrationHttp { progress: progress.clone(), metrics: None }.router();
        let response = router
            .clone()
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        progress
            .update(|s| {
                s.schema_ready = true;
                s.worker_available = true;
                s.phase = MigrationPhase::Reconciling;
            })
            .unwrap();
        progress.finish(Err(MigrationError::Database { sqlstate: None })).unwrap();
        for path in ["/healthz", "/readyz", "/status", "/metrics"] {
            let response = router
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            if path == "/status" {
                let bytes = to_bytes(response.into_body(), 10000).await.unwrap();
                let status: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(status["state"], "failed");
                assert_eq!(status["complete"], false);
                assert!(status.get("backend").is_none());
            }
        }
    }
}
