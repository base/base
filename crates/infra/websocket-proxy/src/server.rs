use std::{
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
use http::{HeaderMap, HeaderValue};
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

#[derive(Clone)]
struct ServerState {
    registry: Registry,
    rate_limiter: Arc<dyn RateLimit>,
    metrics: Arc<Metrics>,
    auth: Authentication,
    ip_addr_http_header: String,
}

#[derive(Clone)]
pub struct Server {
    listen_addr: SocketAddr,
    registry: Registry,
    rate_limiter: Arc<dyn RateLimit>,
    metrics: Arc<Metrics>,
    ip_addr_http_header: String,
    authentication: Option<Authentication>,
    public_access_enabled: bool,
}

#[derive(Deserialize)]
struct FilterQuery {
    addresses: Option<String>,
    topics: Option<String>,
    r#match: Option<String>,
}

impl Server {
    pub fn new(
        listen_addr: SocketAddr,
        registry: Registry,
        metrics: Arc<Metrics>,
        rate_limiter: Arc<dyn RateLimit>,
        authentication: Option<Authentication>,
        ip_addr_http_header: String,
        public_access_enabled: bool,
    ) -> Self {
        Self {
            listen_addr,
            registry,
            rate_limiter,
            metrics,
            authentication,
            ip_addr_http_header,
            public_access_enabled,
        }
    }

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
            rate_limiter: self.rate_limiter.clone(),
            metrics: self.metrics.clone(),
            auth: self.authentication.clone().unwrap_or_else(Authentication::none),
            ip_addr_http_header: self.ip_addr_http_header.clone(),
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
            warn!("Invalid match parameter '{}', defaulting to 'any'", other);
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
    let application = state.auth.get_application_for_key(&api_key);

    match application {
        None => {
            state.metrics.unauthorized_requests.increment(1);

            Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::from(json!({"message": "Invalid API key"}).to_string()))
                .unwrap()
        }
        Some(app) => {
            state.metrics.proxy_connections_by_app(app);
            websocket_handler(state, ws, addr, headers, FilterType::None)
        }
    }
}

async fn authenticated_filter_websocket_handler(
    State(state): State<ServerState>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(api_key): Path<String>,
    query: Query<FilterQuery>,
) -> impl IntoResponse {
    let application = state.auth.get_application_for_key(&api_key);

    match application {
        None => {
            state.metrics.unauthorized_requests.increment(1);

            Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::from(json!({"message": "Invalid API key"}).to_string()))
                .unwrap()
        }
        Some(app) => {
            state.metrics.proxy_connections_by_app(app);
            let filter = create_filter_from_query(query.0);
            websocket_handler(state, ws, addr, headers, filter)
        }
    }
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

    let client_addr = match headers.get(state.ip_addr_http_header) {
        None => connect_addr,
        Some(value) => extract_addr(value, connect_addr),
    };

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
                    state.metrics.per_ip_rate_limited_requests.increment(1);
                }
                RateLimitType::Global => {
                    state.metrics.global_rate_limited_requests.increment(1);
                }
            }

            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(Body::from(json!({"message": reason}).to_string()))
                .unwrap();
        }
    };

    ws.on_failed_upgrade(move |e: Error| {
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

fn extract_addr(header: &HeaderValue, fallback: IpAddr) -> IpAddr {
    if header.is_empty() {
        return fallback;
    }

    match header.to_str() {
        Ok(header_value) => {
            let raw_value = header_value.split(',').map(|ip| ip.trim().to_string()).next_back();

            if let Some(raw_value) = raw_value {
                return raw_value.parse::<IpAddr>().unwrap_or(fallback);
            }

            fallback
        }
        Err(e) => {
            warn!(message = "could not get header value", error = e.to_string());
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

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
        let empty = Some("".to_string());
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

    #[tokio::test]
    async fn test_header_addr() {
        let fb = Ipv4Addr::new(127, 0, 0, 1);

        let test = |header: &str, expected: Ipv4Addr| {
            let hv = HeaderValue::from_str(header).unwrap();
            let result = extract_addr(&hv, IpAddr::V4(fb));
            assert_eq!(result, expected);
        };

        test("129.1.1.1", Ipv4Addr::new(129, 1, 1, 1));
        test("129.1.1.1,130.1.1.1", Ipv4Addr::new(130, 1, 1, 1));
        test("129.1.1.1  ,  130.1.1.1   ", Ipv4Addr::new(130, 1, 1, 1));
        test("nonsense", fb);
        test("400.0.0.1", fb);
        test("120.0.0.1.0", fb);
    }
}
