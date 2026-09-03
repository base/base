//! Axum HTTP server exposing the profiling endpoint, including the `?seconds=&frequency=`
//! query extractor that parameterises a capture.

use std::{future::Future, time::Duration};

use axum::{
    Router,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::{CpuProfiler, ProfilerError};

/// HTTP errors returned by the profiling endpoint.
#[derive(Debug, thiserror::Error)]
pub enum ProfilingServerError {
    /// Another CPU profile capture is already active.
    #[error("cpu profile capture already in progress")]
    Busy,
    /// A capture parameter is outside the supported range.
    #[error("{0}")]
    InvalidParameter(ProfilerError),
    /// The profiler failed while capturing or encoding a profile.
    #[error("{0}")]
    Internal(ProfilerError),
}

impl From<ProfilerError> for ProfilingServerError {
    fn from(error: ProfilerError) -> Self {
        match error {
            ProfilerError::Busy => Self::Busy,
            error @ (ProfilerError::DurationTooLong { .. }
            | ProfilerError::InvalidFrequency { .. }) => Self::InvalidParameter(error),
            error @ (ProfilerError::Pprof(_)
            | ProfilerError::ProtobufEncode { .. }
            | ProfilerError::TaskJoin { .. }
            | ProfilerError::Gzip(_)) => Self::Internal(error),
        }
    }
}

impl IntoResponse for ProfilingServerError {
    fn into_response(self) -> Response {
        match self {
            Self::Busy => {
                (StatusCode::CONFLICT, "cpu profile capture already in progress\n").into_response()
            }
            Self::InvalidParameter(error) => {
                (StatusCode::BAD_REQUEST, format!("{error}\n")).into_response()
            }
            Self::Internal(error) => {
                error!(error = %error, "cpu profile request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error\n").into_response()
            }
        }
    }
}

/// Dedicated HTTP server for on-demand CPU profile captures.
#[derive(Debug, Clone)]
pub struct ProfilingServer {
    port: u16,
    profiler: CpuProfiler,
    cancel: CancellationToken,
}

impl ProfilingServer {
    /// Creates a profiling server on the dedicated profiling `port`.
    pub const fn new(port: u16, profiler: CpuProfiler, cancel: CancellationToken) -> Self {
        Self { port, profiler, cancel }
    }

    /// Serves profiling requests until the cancellation token is triggered.
    ///
    /// # Errors
    ///
    /// Returns an error when the TCP listener cannot bind or the HTTP server exits unexpectedly.
    pub async fn serve(self) -> eyre::Result<()> {
        // Compose port class controls exposure; binding 0.0.0.0 inside the container keeps host
        // capture reachable, while 127.0.0.1 would trap the endpoint inside the container.
        let address = format!("0.0.0.0:{}", self.port);
        let listener = TcpListener::bind(address).await?;
        let local_address = listener.local_addr()?;
        let app = profile_router(self.profiler);
        let cancel = self.cancel;
        info!(address = %local_address, "profiling server started");

        axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await?;

        info!(address = %local_address, "profiling server stopped");
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct ProfileQuery {
    seconds: Option<u64>,
    frequency: Option<u32>,
}

#[derive(Debug, Clone)]
struct ProfileState<P> {
    profiler: P,
}

trait ProfileCapture: Clone + Send + Sync + 'static {
    fn max_capture_seconds(&self) -> u64;

    fn capture(
        &self,
        duration: Duration,
        frequency: Option<u32>,
    ) -> impl Future<Output = Result<Vec<u8>, ProfilerError>> + Send;
}

impl ProfileCapture for CpuProfiler {
    fn max_capture_seconds(&self) -> u64 {
        Self::max_capture_seconds(self)
    }

    fn capture(
        &self,
        duration: Duration,
        frequency: Option<u32>,
    ) -> impl Future<Output = Result<Vec<u8>, ProfilerError>> + Send {
        Self::capture(self, duration, frequency)
    }
}

fn profile_router<P: ProfileCapture>(profiler: P) -> Router {
    Router::new()
        .route("/debug/pprof/profile", get(capture_profile::<P>))
        .with_state(ProfileState { profiler })
}

async fn capture_profile<P: ProfileCapture>(
    State(state): State<ProfileState<P>>,
    Query(query): Query<ProfileQuery>,
) -> Result<Response, ProfilingServerError> {
    let default_seconds = 30.min(state.profiler.max_capture_seconds());
    let duration = Duration::from_secs(query.seconds.unwrap_or(default_seconds));
    let profile = state.profiler.capture(duration, query.frequency).await?;

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"profile.pb.gz\""),
        ],
        profile,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    //! A hand-rolled profiler fake keeps HTTP tests deterministic because `pprof` permits only one
    //! process-wide capture, so real captures in parallel test threads would contend globally.

    use std::{
        future::Future,
        sync::{Arc, Mutex as StdMutex},
        time::Duration,
    };

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use tokio::{
        net::TcpStream,
        sync::{Mutex, Notify},
        task::yield_now,
        time::timeout,
    };
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use super::*;

    const GZIP_PROFILE: &[u8] = &[0x1f, 0x8b, 0x08, 0x00];
    type Captures = Arc<StdMutex<Vec<(Duration, Option<u32>)>>>;

    #[derive(Debug, Clone)]
    struct FakeProfiler {
        capture_lock: Arc<Mutex<()>>,
        captures: Captures,
        max_capture_seconds: u64,
        started: Arc<Notify>,
        release: Option<CancellationToken>,
    }

    impl FakeProfiler {
        fn immediate() -> Self {
            Self {
                capture_lock: Arc::default(),
                captures: Arc::default(),
                max_capture_seconds: 60,
                started: Arc::default(),
                release: None,
            }
        }

        fn with_max_capture_seconds(max_capture_seconds: u64) -> Self {
            Self { max_capture_seconds, ..Self::immediate() }
        }

        fn blocking() -> Self {
            Self { release: Some(CancellationToken::new()), ..Self::immediate() }
        }
    }

    impl ProfileCapture for FakeProfiler {
        fn max_capture_seconds(&self) -> u64 {
            self.max_capture_seconds
        }

        fn capture(
            &self,
            duration: Duration,
            frequency: Option<u32>,
        ) -> impl Future<Output = Result<Vec<u8>, ProfilerError>> + Send {
            let capture_lock = Arc::clone(&self.capture_lock);
            let captures = Arc::clone(&self.captures);
            let started = Arc::clone(&self.started);
            let release = self.release.clone();

            async move {
                let _permit = capture_lock.try_lock().map_err(|_| ProfilerError::Busy)?;
                captures.lock().unwrap().push((duration, frequency));
                started.notify_one();
                if let Some(release) = release {
                    release.cancelled().await;
                }
                Ok(GZIP_PROFILE.to_vec())
            }
        }
    }

    fn request(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn profile_route_returns_gzip_attachment() {
        let profiler = FakeProfiler::immediate();
        let app = profile_router(profiler.clone());
        let request = request("/debug/pprof/profile?seconds=1&frequency=101");

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/octet-stream");
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"profile.pb.gz\""
        );
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..2], &[0x1f, 0x8b]);
        assert_eq!(
            profiler.captures.lock().unwrap().as_slice(),
            &[(Duration::from_secs(1), Some(101))]
        );
    }

    #[tokio::test]
    async fn profile_route_leaves_omitted_frequency_unset() {
        let profiler = FakeProfiler::immediate();
        let app = profile_router(profiler.clone());
        let request = request("/debug/pprof/profile?seconds=1");

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(profiler.captures.lock().unwrap().as_slice(), &[(Duration::from_secs(1), None)]);
    }

    #[tokio::test]
    async fn profile_route_clamps_omitted_seconds_to_profiler_maximum() {
        let profiler = FakeProfiler::with_max_capture_seconds(10);
        let app = profile_router(profiler.clone());
        let request = request("/debug/pprof/profile");

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            profiler.captures.lock().unwrap().as_slice(),
            &[(Duration::from_secs(10), None)]
        );
    }

    #[tokio::test]
    async fn profile_route_returns_conflict_while_capture_is_active() {
        let profiler = FakeProfiler::blocking();
        let app = profile_router(profiler.clone());
        let first_request = request("/debug/pprof/profile?seconds=1");
        let first_capture = tokio::spawn(app.clone().oneshot(first_request));
        profiler.started.notified().await;
        let second_request = request("/debug/pprof/profile?seconds=1");

        let second_response = app.oneshot(second_request).await.unwrap();

        assert_eq!(second_response.status(), StatusCode::CONFLICT);
        profiler.release.as_ref().unwrap().cancel();
        assert_eq!(first_capture.await.unwrap().unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn profile_route_rejects_out_of_range_frequency() {
        let app = profile_router(CpuProfiler::default());
        let request = request("/debug/pprof/profile?seconds=1&frequency=0");

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn serve_resolves_after_cancellation() {
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reserved.local_addr().unwrap().port();
        drop(reserved);
        let cancel = CancellationToken::new();
        let server = ProfilingServer::new(port, CpuProfiler::default(), cancel.clone());
        let serving = tokio::spawn(server.serve());
        let address = ("127.0.0.1", port);
        let connection = timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(connection) = TcpStream::connect(address).await {
                    break connection;
                }
                yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(connection);

        cancel.cancel();

        let result = timeout(Duration::from_secs(1), serving).await.unwrap().unwrap();
        assert!(result.is_ok());
    }
}
