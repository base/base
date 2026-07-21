use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use axum::{
    Error, Router,
    body::Body,
    extract::{ConnectInfo, Path, Query, State, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{any, get},
};
use http::HeaderMap;
use ipnet::IpNet;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    auth::Authentication,
    client::ClientConnection,
    filter::{FilterType, MatchMode},
    metrics::Metrics,
    rate_limit::{RateLimit, RateLimitError, RateLimitType},
    registry::Registry,
};

/// Configuration for resolving client IPs through trusted proxies.
#[derive(Clone, Debug)]
pub struct TrustedProxyConfig {
    ip_addr_http_header: String,
    trusted_proxy_cidrs: Vec<IpNet>,
}

impl TrustedProxyConfig {
    /// Creates a trusted proxy configuration for the given header and proxy CIDRs.
    pub const fn new(ip_addr_http_header: String, trusted_proxy_cidrs: Vec<IpNet>) -> Self {
        Self { ip_addr_http_header, trusted_proxy_cidrs }
    }

    /// Resolves the client IP, trusting forwarding headers only from configured proxy CIDRs.
    pub fn client_ip(&self, connect_addr: IpAddr, headers: &HeaderMap) -> IpAddr {
        // Dual-stack listeners present IPv4 peers as IPv4-mapped IPv6 (`::ffff:x.x.x.x`).
        // Canonicalize so IPv4 CIDRs still match those peers and rate-limit buckets stay
        // consistent across address forms.
        let connect_addr = Self::canonicalize_ip(connect_addr);

        if !self.trusted_proxy_cidrs.iter().any(|cidr| cidr.contains(&connect_addr)) {
            return connect_addr;
        }

        let Some(header) = headers.get(&self.ip_addr_http_header) else {
            return connect_addr;
        };

        let header_value = match header.to_str() {
            Ok(header_value) => header_value,
            Err(error) => {
                warn!(error = %error, "Could not read client IP header");
                return connect_addr;
            }
        };

        header_value
            .split(',')
            .next_back()
            .and_then(|ip| {
                let trimmed = ip.trim();
                match trimmed.parse::<IpAddr>() {
                    Ok(addr) => Some(Self::canonicalize_ip(addr)),
                    Err(error) => {
                        warn!(
                            error = %error,
                            value = %trimmed,
                            "Failed to parse forwarded client IP"
                        );
                        None
                    }
                }
            })
            .unwrap_or(connect_addr)
    }

    fn canonicalize_ip(addr: IpAddr) -> IpAddr {
        match addr {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(addr),
            addr => addr,
        }
    }
}

#[derive(Clone)]
struct ServerState {
    registry: Registry,
    rate_limiter: Arc<dyn RateLimit>,
    auth: Authentication,
    trusted_proxy_config: TrustedProxyConfig,
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

#[derive(Deserialize)]
struct FilterQuery {
    addresses: Option<String>,
    topics: Option<String>,
    r#match: Option<String>,
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
            router = router
                .route("/ws/{api_key}", any(authenticated_websocket_handler))
                .route("/ws/{api_key}/filter", any(authenticated_filter_websocket_handler));
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

// Parse comma-separated values into Vec<String>
fn parse_comma_separated(input: Option<String>) -> Vec<String> {
    input
        .map(|s| {
            s.split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn create_filter_from_query(query: FilterQuery) -> FilterType {
    let addresses = parse_comma_separated(query.addresses);
    let topics = parse_comma_separated(query.topics);

    // Parse match mode, default to "any" if not specified
    let match_mode = match query.r#match.as_deref() {
        Some("all") => MatchMode::All,
        Some("any") | None => MatchMode::Any,
        Some(other) => {
            warn!(param = %other, "Invalid match parameter, defaulting to 'any'");
            MatchMode::Any
        }
    };

    let filter = FilterType::new_combined_with_mode(addresses.clone(), topics.clone(), match_mode);
    debug!(
        "Created filter: {:?} from addresses: {:?}, topics: {:?}, match_mode: {:?}",
        filter, addresses, topics, match_mode
    );
    filter
}

async fn authenticated_websocket_handler(
    State(state): State<ServerState>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(api_key): Path<String>,
) -> impl IntoResponse {
    let application = state.auth.get_application_for_key(&api_key).cloned();

    application.map_or_else(
        || {
            Metrics::unauthorized_requests().increment(1);

            Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::from(json!({"message": "Invalid API key"}).to_string()))
                .unwrap()
        },
        |app| {
            Metrics::connections_by_app(app).increment(1);
            websocket_handler(state, ws, addr, headers, FilterType::None)
        },
    )
}

async fn authenticated_filter_websocket_handler(
    State(state): State<ServerState>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(api_key): Path<String>,
    query: Query<FilterQuery>,
) -> impl IntoResponse {
    let application = state.auth.get_application_for_key(&api_key).cloned();

    application.map_or_else(
        || {
            Metrics::unauthorized_requests().increment(1);

            Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::from(json!({"message": "Invalid API key"}).to_string()))
                .unwrap()
        },
        |app| {
            Metrics::connections_by_app(app).increment(1);
            let filter = create_filter_from_query(query.0);
            websocket_handler(state, ws, addr, headers, filter)
        },
    )
}

async fn unauthenticated_websocket_handler(
    State(state): State<ServerState>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    websocket_handler(state, ws, addr, headers, FilterType::None)
}

fn websocket_handler(
    state: ServerState,
    ws: WebSocketUpgrade,
    addr: SocketAddr,
    headers: HeaderMap,
    filter: FilterType,
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

    // Downstream clients are primarily receive-only (flashblock broadcast). Limit inbound
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
            let client = ClientConnection::new(client_addr, ticket, socket, filter);
            state.registry.subscribe(client).await;
        })
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use http::HeaderValue;

    use super::*;
    use crate::filter::FilterType;

    #[test]
    fn test_parse_comma_separated() {
        // Test single value
        let single = Some("0x123".to_string());
        assert_eq!(parse_comma_separated(single), vec!["0x123"]);

        // Test multiple values
        let multiple = Some("0x123, 0x456 ,0x789".to_string());
        assert_eq!(parse_comma_separated(multiple), vec!["0x123", "0x456", "0x789"]);

        // Test empty
        assert_eq!(parse_comma_separated(None), Vec::<String>::new());

        // Test empty string
        let empty = Some(String::new());
        assert_eq!(parse_comma_separated(empty), Vec::<String>::new());

        // Test with empty values
        let with_empty = Some("0x123,,0x456".to_string());
        assert_eq!(parse_comma_separated(with_empty), vec!["0x123", "0x456"]);
    }

    #[test]
    fn test_create_filter_from_query() {
        // Test addresses only
        let query =
            FilterQuery { addresses: Some("0x123,0x456".to_string()), topics: None, r#match: None };
        let filter = create_filter_from_query(query);
        match filter {
            FilterType::Addresses(_) => (),
            _ => panic!("Expected Addresses filter"),
        }

        // Test topics only
        let query =
            FilterQuery { addresses: None, topics: Some("0xabc,0xdef".to_string()), r#match: None };
        let filter = create_filter_from_query(query);
        match filter {
            FilterType::Topics(_) => (),
            _ => panic!("Expected Topics filter"),
        }

        // Test combined
        let query = FilterQuery {
            addresses: Some("0x123".to_string()),
            topics: Some("0xabc".to_string()),
            r#match: Some("all".to_string()),
        };
        let filter = create_filter_from_query(query);
        match filter {
            FilterType::Combined { .. } => (),
            _ => panic!("Expected Combined filter"),
        }

        // Test none
        let query = FilterQuery { addresses: None, topics: None, r#match: None };
        let filter = create_filter_from_query(query);
        match filter {
            FilterType::None => (),
            _ => panic!("Expected None filter"),
        }
    }

    #[test]
    fn trusted_proxy_config_resolves_client_ip() {
        let config = TrustedProxyConfig::new(
            "x-forwarded-for".to_string(),
            vec!["127.0.0.0/8".parse().unwrap()],
        );
        let trusted_proxy = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let untrusted_peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let client = IpAddr::V4(Ipv4Addr::new(130, 1, 1, 1));

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("129.1.1.1, 130.1.1.1"));

        assert_eq!(config.client_ip(trusted_proxy, &headers), client);
        assert_eq!(config.client_ip(untrusted_peer, &headers), untrusted_peer);

        headers.insert("x-forwarded-for", HeaderValue::from_static("nonsense"));
        assert_eq!(config.client_ip(trusted_proxy, &headers), trusted_proxy);

        headers.clear();
        assert_eq!(config.client_ip(trusted_proxy, &headers), trusted_proxy);
    }

    #[test]
    fn trusted_proxy_config_matches_ipv4_mapped_peers() {
        let config = TrustedProxyConfig::new(
            "x-forwarded-for".to_string(),
            vec!["10.0.0.0/8".parse().unwrap()],
        );
        let mapped_proxy = IpAddr::V6("::ffff:10.0.0.1".parse().unwrap());
        let client = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.10"));

        assert_eq!(config.client_ip(mapped_proxy, &headers), client);

        headers.insert("x-forwarded-for", HeaderValue::from_static("::ffff:203.0.113.10"));
        assert_eq!(config.client_ip(mapped_proxy, &headers), client);
    }
}
