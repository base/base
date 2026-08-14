//! Controllable L1 JSON-RPC forwarding for system-test fault injection.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{Response, StatusCode},
    routing::post,
};
use eyre::{Result, WrapErr};
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;

/// An HTTP proxy that can make the configured L1 JSON-RPC endpoint unavailable on demand.
#[derive(Debug)]
pub struct L1RpcProxy {
    url: Url,
    available: Arc<AtomicBool>,
    rejected_requests: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl L1RpcProxy {
    /// Starts a proxy forwarding JSON-RPC requests to `upstream`.
    pub async fn start(upstream: Url) -> Result<Self> {
        let listener =
            TcpListener::bind("127.0.0.1:0").await.wrap_err("Failed to bind L1 RPC fault proxy")?;
        let address = listener.local_addr().wrap_err("Failed to read L1 RPC proxy address")?;
        let url = Url::parse(&format!("http://{address}"))?;
        let available = Arc::new(AtomicBool::new(true));
        let handler_available = Arc::clone(&available);
        let rejected_requests = Arc::new(AtomicU64::new(0));
        let handler_rejected_requests = Arc::clone(&rejected_requests);
        let client = reqwest::Client::new();

        let app = Router::new().route(
            "/",
            post(move |body: Bytes| {
                let available = Arc::clone(&handler_available);
                let rejected_requests = Arc::clone(&handler_rejected_requests);
                let client = client.clone();
                let upstream = upstream.clone();
                async move {
                    if !available.load(Ordering::Acquire) {
                        rejected_requests.fetch_add(1, Ordering::Relaxed);
                        return Response::builder()
                            .status(StatusCode::SERVICE_UNAVAILABLE)
                            .body(Body::from("L1 RPC fault injected"))
                            .expect("static L1 RPC outage response should be valid");
                    }

                    match client
                        .post(upstream)
                        .header("content-type", "application/json")
                        .body(body)
                        .send()
                        .await
                    {
                        Ok(response) => {
                            let status = response.status();
                            match response.bytes().await {
                                Ok(body) => Response::builder()
                                    .status(status)
                                    .header("content-type", "application/json")
                                    .body(Body::from(body))
                                    .expect("forwarded L1 RPC response should be valid"),
                                Err(error) => Response::builder()
                                    .status(StatusCode::BAD_GATEWAY)
                                    .body(Body::from(error.to_string()))
                                    .expect("L1 RPC body error response should be valid"),
                            }
                        }
                        Err(error) => Response::builder()
                            .status(StatusCode::BAD_GATEWAY)
                            .body(Body::from(error.to_string()))
                            .expect("L1 RPC forwarding error response should be valid"),
                    }
                }
            }),
        );
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::warn!(error = %error, "L1 RPC fault proxy stopped unexpectedly");
            }
        });

        Ok(Self { url, available, rejected_requests, task })
    }

    /// Returns the proxy URL.
    pub const fn url(&self) -> &Url {
        &self.url
    }

    /// Rejects all subsequent L1 RPC requests.
    pub fn disable(&self) {
        self.available.store(false, Ordering::Release);
    }

    /// Resumes forwarding L1 RPC requests.
    pub fn enable(&self) {
        self.available.store(true, Ordering::Release);
    }

    /// Returns whether requests are currently forwarded.
    pub fn is_available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }

    /// Returns the number of requests rejected while fault injection was active.
    pub fn rejected_requests(&self) -> u64 {
        self.rejected_requests.load(Ordering::Relaxed)
    }
}

impl Drop for L1RpcProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}
