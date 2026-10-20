use std::{fmt, net::SocketAddr, sync::Arc};

use axum::{
    Error, Router,
    body::Body,
    extract::{ConnectInfo, Path, State, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{any, get},
};
pub use base_trusted_proxy::TrustedProxyConfig;
use http::HeaderMap;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    auth::Authentication,
    client::ClientConnection,
    metrics::Metrics,
    rate_limit::{RateLimit, RateLimitError, RateLimitType},
    registry::Registry,
};

#[derive(Clone)]
struct ServerState {
    registry: Registry,
    rate_limiter: Arc<dyn RateLimit>,
    auth: Authentication,
    trusted_proxy_config: TrustedProxyConfig,
}

impl ServerState {
    /// Validates the API key, counting either an unauthorized request or a successful
    /// connection for the key's application, and returns the 401 response for invalid keys.
    fn authenticate(&self, api_key: &str) -> Result<(), Box<Response>> {
        let Some(app) = self.auth.get_application_for_key(api_key).cloned() else {
            Metrics::unauthorized_requests().increment(1);

            return Err(Box::new(
                Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Body::from(json!({"message": "Invalid API key"}).to_string()))
                    .unwrap(),
            ));
        };

        Metrics::connections_by_app(app).increment(1);
        Ok(())
    }
}

/// WebSocket proxy server that accepts client connections and forwards messages
/// from a shared registry of upstream sources.
#[derive(Clone)]
pub struct Server {
    listen_addr: SocketAddr,
    registry: Registry,
    rate_limiter: Arc<dyn RateLimit>,
    trusted_proxy_config: TrustedProxyConfig,
    authentication: Option<Authentication>,
    public_access_enabled: bool,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("listen_addr", &self.listen_addr)
            .field("registry", &self.registry)
            .field("trusted_proxy_config", &self.trusted_proxy_config)
            .field("authentication", &self.authentication)
            .field("public_access_enabled", &self.public_access_enabled)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Creates a new server with the given configuration.
    pub fn new(
        listen_addr: SocketAddr,
        registry: Registry,
        rate_limiter: Arc<dyn RateLimit>,
        authentication: Option<Authentication>,
        trusted_proxy_config: TrustedProxyConfig,
        public_access_enabled: bool,
    ) -> Self {
        Self {
            listen_addr,
            registry,
            rate_limiter,
            authentication,
            trusted_proxy_config,
            public_access_enabled,
        }
    }

    /// Starts the HTTP/WebSocket server and listens for incoming connections
    /// until the cancellation token is triggered.
    pub async fn listen(&self, cancellation_token: CancellationToken) {
        let mut router: Router<ServerState> = Router::new().route("/healthz", get(healthz_handler));

        if self.authentication.is_some() {
            info!("Authentication is enabled");
            router = router.route("/ws/{api_key}", any(authenticated_websocket_handler));
        } else {
            info!("Public endpoint is enabled");
            router = router.route("/ws", any(unauthenticated_websocket_handler));
        }

        if self.public_access_enabled && self.authentication.is_some() {
            info!("Public endpoint is enabled");
            router = router.route("/ws", any(unauthenticated_websocket_handler));
        }

        let router = router.with_state(ServerState {
            registry: self.registry.clone(),
            rate_limiter: Arc::clone(&self.rate_limiter),
            auth: self.authentication.clone().unwrap_or_else(Authentication::none),
            trusted_proxy_config: self.trusted_proxy_config.clone(),
        });

        let listener = tokio::net::TcpListener::bind(self.listen_addr).await.unwrap();

        info!(message = "starting server", address = listener.local_addr().unwrap().to_string());

        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(cancellation_token.cancelled_owned())
            .await
            .unwrap()
    }
}

async fn healthz_handler() -> impl IntoResponse {
    StatusCode::OK
}

async fn authenticated_websocket_handler(
    State(state): State<ServerState>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(api_key): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = state.authenticate(&api_key) {
        return *response;
    }

    websocket_handler(state, ws, addr, headers)
}

async fn unauthenticated_websocket_handler(
    State(state): State<ServerState>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    websocket_handler(state, ws, addr, headers)
}

fn websocket_handler(
    state: ServerState,
    ws: WebSocketUpgrade,
    addr: SocketAddr,
    headers: HeaderMap,
) -> Response {
    let connect_addr = addr.ip();
    let client_addr = state.trusted_proxy_config.client_ip(connect_addr, &headers);

    let ticket = match state.rate_limiter.try_acquire(client_addr) {
        Ok(ticket) => ticket,
        Err(RateLimitError::Limit { reason, limit_type }) => {
            match limit_type {
                RateLimitType::PerIp => {
                    info!(
                        message = "per-IP rate limit exceeded",
                        client_ip = client_addr.to_string(),
                        reason = reason
                    );
                    Metrics::per_ip_rate_limited_requests().increment(1);
                }
                RateLimitType::Global => {
                    Metrics::global_rate_limited_requests().increment(1);
                }
            }

            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(Body::from(json!({"message": reason}).to_string()))
                .unwrap();
        }
    };

    // Downstream clients are primarily receive-only (block broadcast). Limit inbound
    // frame and message sizes to prevent clients from forcing large allocations
    // that the proxy never uses. Only small control frames (ping, pong, close) are
    // expected from clients.
    const MAX_CLIENT_MESSAGE_SIZE: usize = 4 * 1024;
    const MAX_CLIENT_FRAME_SIZE: usize = 4 * 1024;

    ws.max_message_size(MAX_CLIENT_MESSAGE_SIZE)
        .max_frame_size(MAX_CLIENT_FRAME_SIZE)
        .on_failed_upgrade(move |e: Error| {
            info!(
                message = "failed to upgrade connection",
                error = e.to_string(),
                client = addr.to_string()
            )
        })
        .on_upgrade(async move |socket| {
            let client = ClientConnection::new(client_addr, ticket, socket);
            state.registry.subscribe(client).await;
        })
}
