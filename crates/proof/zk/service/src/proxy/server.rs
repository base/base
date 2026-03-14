use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    extract::{Request as AxumRequest, State},
    http::{Response, StatusCode},
    response::IntoResponse,
};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};

use super::{config::ProxyConfig, rate_limit::RateLimiter};

/// Shared state for the proxy server
#[derive(Clone)]
struct ProxyState {
    backend_url: String,
    rate_limiter: RateLimiter,
    client: reqwest::Client,
}

/// Start a proxy server with the given configuration
pub(super) async fn start_proxy(config: ProxyConfig) -> anyhow::Result<()> {
    config.validate()?;

    let rate_limiter = RateLimiter::new(
        config.requests_per_second,
        config.max_concurrent_requests,
        config.queue_timeout,
        format!("proxy-{}", config.local_port),
    )?;

    let state = ProxyState {
        backend_url: config.backend_url.clone(),
        rate_limiter,
        client: reqwest::Client::builder().timeout(std::time::Duration::from_secs(120)).build()?,
    };

    let app = Router::new()
        .fallback(handle_request)
        .layer(TraceLayer::new_for_http())
        .with_state(Arc::new(state));

    let addr = format!("127.0.0.1:{}", config.local_port);
    let listener = TcpListener::bind(&addr).await?;

    info!(
        addr = %addr,
        backend_url = %config.backend_url,
        requests_per_second = config.requests_per_second,
        max_concurrent_requests = config.max_concurrent_requests,
        "Proxy server started"
    );

    axum::serve(listener, app).await?;

    Ok(())
}

/// Handle incoming requests (both JSON-RPC and REST API)
async fn handle_request(
    State(state): State<Arc<ProxyState>>,
    request: AxumRequest,
) -> impl IntoResponse {
    // Acquire rate limit permit
    let _permit = match state.rate_limiter.acquire().await {
        Some(permit) => permit,
        None => {
            warn!("Rate limit exceeded, returning 429");
            return (StatusCode::TOO_MANY_REQUESTS, "Rate limit exceeded. Please retry later.")
                .into_response();
        }
    };

    // Extract method, path, and query
    let method = request.method().clone();
    let uri = request.uri().clone();
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");

    // Build full backend URL with path and query
    let backend_url = format!("{}{}", state.backend_url.trim_end_matches('/'), path_and_query);

    debug!(
        method = %method,
        path_and_query = %path_and_query,
        backend_url = %backend_url,
        "Proxying request"
    );

    // Extract headers to forward
    let headers = request.headers().clone();

    // Extract the body
    let body_bytes = match axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(err) => {
            error!(error = %err, "Failed to read request body");
            return (StatusCode::BAD_REQUEST, "Failed to read request body").into_response();
        }
    };

    debug!(body_size = body_bytes.len(), "Request body size");

    // Build request to backend with the appropriate method
    let mut backend_request = state.client.request(method.clone(), &backend_url);

    // Forward relevant headers (skip host, connection, etc.)
    for (key, value) in &headers {
        let key_str = key.as_str();
        if !should_skip_header(key_str) {
            if let Ok(val_str) = value.to_str() {
                debug!(header_key = key_str, header_value = val_str, "Forwarding header");
            }
            backend_request = backend_request.header(key, value);
        }
    }

    // Add body if present (for POST, PUT, etc.)
    if !body_bytes.is_empty() {
        backend_request = backend_request.body(body_bytes);
    }

    // Send request to backend
    let response = match backend_request.send().await {
        Ok(resp) => resp,
        Err(err) => {
            error!(
                backend_url = %backend_url,
                error = %err,
                "Failed to forward request to backend"
            );
            return (StatusCode::BAD_GATEWAY, format!("Failed to connect to backend: {err}"))
                .into_response();
        }
    };

    // Get response status and headers
    let status = response.status();
    let response_headers = response.headers().clone();

    debug!(status = %status, "Backend response status");
    debug!(headers = ?response_headers, "Backend response headers");

    // Get response body
    let response_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(err) => {
            error!(error = %err, "Failed to read backend response");
            return (StatusCode::BAD_GATEWAY, "Failed to read backend response").into_response();
        }
    };

    debug!(body_size = response_bytes.len(), "Backend response body size");

    // Log response body for debugging (only first 1000 bytes)
    if response_bytes.len() <= 1000 {
        let body_str = String::from_utf8_lossy(&response_bytes);
        debug!(body = %body_str, "Backend response body");
    } else {
        let body_str = String::from_utf8_lossy(&response_bytes[..1000]);
        debug!(body = %body_str, "Backend response body (truncated)");
    }

    // Build response with all headers from backend
    let mut builder = Response::builder().status(status);

    for (key, value) in &response_headers {
        let key_str = key.as_str();
        if !should_skip_response_header(key_str) {
            builder = builder.header(key, value);
        }
    }

    match builder.body(Body::from(response_bytes)) {
        Ok(response) => response.into_response(),
        Err(err) => {
            error!(error = %err, "Failed to build response");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
        }
    }
}

/// Determine if a header should be skipped when forwarding requests
fn should_skip_header(name: &str) -> bool {
    matches!(name.to_lowercase().as_str(), "host" | "connection" | "transfer-encoding")
}

/// Determine if a header should be skipped when forwarding responses
fn should_skip_response_header(name: &str) -> bool {
    matches!(name.to_lowercase().as_str(), "connection" | "transfer-encoding")
}
