use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    str::FromStr,
    sync::{Arc, Mutex, PoisonError},
};

use alloy_primitives::B512;
use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use ipnet::IpNet;
use secp256k1::PublicKey;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, info};

use crate::prober::{ReachabilityProber, RlpxProbeOutcome, RlpxProbeStage, RlpxProbeTarget};

/// HTTP path for execution-layer P2P reachability checks.
pub const P2P_REACHABILITY_PATH: &str = "/v1/p2p/reachability/el";
/// Maximum JSON request body size accepted by the reachability route.
pub const P2P_REACHABILITY_MAX_REQUEST_BYTES: usize = 1024;
/// Maximum number of reachability probes allowed in flight globally.
pub const P2P_REACHABILITY_MAX_CONCURRENT_PROBES: usize = 32;
#[cfg(test)]
/// Valid execution-layer node identity shared by reachability tests.
pub const TEST_NODE_ID: &str = "2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4";

/// Public address family the caller expects the service to observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum P2pAddressFamily {
    /// `IPv4` source and probe target.
    Ipv4,
    /// `IPv6` source and probe target.
    Ipv6,
}

impl P2pAddressFamily {
    /// Returns whether an observed source IP matches this family.
    pub const fn matches(self, ip: IpAddr) -> bool {
        matches!((self, ip), (Self::Ipv4, IpAddr::V4(_)) | (Self::Ipv6, IpAddr::V6(_)))
    }
}

/// JSON request for an execution-layer reachability check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct P2pReachabilityRequest {
    /// Expected 64-byte `RLPx` node identity, encoded as 128 hexadecimal characters.
    pub node_id: String,
    /// Public TCP port advertised by the execution-layer node.
    pub tcp_port: u16,
    /// Address family of the node's advertised public endpoint.
    pub address_family: P2pAddressFamily,
}

impl P2pReachabilityRequest {
    /// Validates the request and combines it with the observed source IP.
    pub fn target(&self, source_ip: IpAddr) -> Option<RlpxProbeTarget> {
        if self.tcp_port == 0 || !self.address_family.matches(source_ip) {
            return None;
        }

        let raw_node_id = self.node_id.strip_prefix("0x").unwrap_or(&self.node_id);
        let node_id = B512::from_str(raw_node_id).ok()?;

        let mut encoded_public_key = [0_u8; 65];
        encoded_public_key[0] = 4;
        encoded_public_key[1..].copy_from_slice(node_id.as_slice());
        PublicKey::from_slice(&encoded_public_key).ok()?;

        Some(RlpxProbeTarget { address: SocketAddr::new(source_ip, self.tcp_port), node_id })
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
    /// Public source address and requested TCP port probed by the service.
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
    /// The JSON body, node identity, or port was invalid.
    InvalidRequest,
    /// The request source or forwarding information was invalid.
    InvalidSource,
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
            Self::InvalidRequest | Self::InvalidSource => StatusCode::BAD_REQUEST,
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

impl From<ClientIpError> for P2pApiError {
    fn from(_: ClientIpError) -> Self {
        Self::InvalidSource
    }
}

/// Error resolving the public client IP for a probe request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClientIpError {
    /// An untrusted client supplied `X-Forwarded-For`.
    #[error("untrusted peer supplied X-Forwarded-For")]
    UntrustedForwardedFor,
    /// A trusted proxy did not supply `X-Forwarded-For`.
    #[error("trusted proxy omitted X-Forwarded-For")]
    MissingForwardedFor,
    /// A forwarded hop was empty, non-UTF-8, or not an IP address.
    #[error("malformed X-Forwarded-For chain")]
    MalformedForwardedFor,
    /// Every forwarded hop belonged to a trusted proxy network.
    #[error("X-Forwarded-For chain contains only trusted proxies")]
    AllHopsTrusted,
    /// The resolved client IP was not globally routable.
    #[error("resolved client IP is not globally routable")]
    NonGlobalClientIp,
}

/// Resolves a probe target IP from the socket peer and trusted proxy chain.
#[derive(Debug, Clone)]
pub struct ClientIpResolver {
    trusted_proxy_cidrs: Arc<[IpNet]>,
}

impl ClientIpResolver {
    /// Maximum number of forwarded hops accepted from a trusted proxy.
    pub const MAX_FORWARDED_HOPS: usize = 32;

    /// Creates a resolver with the supplied trusted proxy networks.
    pub fn new(trusted_proxy_cidrs: Vec<IpNet>) -> Self {
        Self { trusted_proxy_cidrs: trusted_proxy_cidrs.into() }
    }

    /// Resolves and validates the public client IP for one request.
    pub fn resolve(
        &self,
        socket_peer: SocketAddr,
        headers: &HeaderMap,
    ) -> Result<IpAddr, ClientIpError> {
        let socket_ip = Self::normalize(socket_peer.ip());
        let forwarded_values = headers.get_all("x-forwarded-for").iter().collect::<Vec<_>>();

        if forwarded_values.is_empty() {
            if self.is_trusted(socket_ip) {
                return Err(ClientIpError::MissingForwardedFor);
            }
            return Self::require_global(socket_ip);
        }

        if !self.is_trusted(socket_ip) {
            return Err(ClientIpError::UntrustedForwardedFor);
        }

        let mut hops = Vec::new();
        for value in forwarded_values {
            let value = value.to_str().map_err(|_| ClientIpError::MalformedForwardedFor)?;
            for raw_hop in value.split(',') {
                let raw_hop = raw_hop.trim();
                if raw_hop.is_empty() {
                    return Err(ClientIpError::MalformedForwardedFor);
                }
                let hop = raw_hop
                    .parse::<IpAddr>()
                    .map(Self::normalize)
                    .map_err(|_| ClientIpError::MalformedForwardedFor)?;
                if hops.len() >= Self::MAX_FORWARDED_HOPS {
                    return Err(ClientIpError::MalformedForwardedFor);
                }
                hops.push(hop);
            }
        }
        hops.push(socket_ip);

        for hop in hops.into_iter().rev() {
            if !self.is_trusted(hop) {
                return Self::require_global(hop);
            }
        }

        Err(ClientIpError::AllHopsTrusted)
    }

    /// Returns whether an IP belongs to a configured trusted proxy network.
    pub fn is_trusted(&self, ip: IpAddr) -> bool {
        self.trusted_proxy_cidrs.iter().any(|network| network.contains(&ip))
    }

    /// Converts `IPv4`-mapped `IPv6` addresses to their canonical `IPv4` form.
    pub fn normalize(ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4),
            IpAddr::V4(ip) => IpAddr::V4(ip),
        }
    }

    /// Requires an address to be globally routable.
    pub fn require_global(ip: IpAddr) -> Result<IpAddr, ClientIpError> {
        if Self::is_global(ip) { Ok(ip) } else { Err(ClientIpError::NonGlobalClientIp) }
    }

    /// Returns whether an address is suitable for a public unicast TCP probe.
    pub fn is_global(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(ip) => Self::is_global_ipv4(ip),
            IpAddr::V6(ip) => Self::is_global_ipv6(ip),
        }
    }

    /// Returns whether an `IPv4` address is suitable for a public unicast TCP probe.
    pub fn is_global_ipv4(ip: Ipv4Addr) -> bool {
        let [a, b, c, d] = ip.octets();
        !(a == 0
            || ip.is_private()
            || (a == 100 && (64..=127).contains(&b))
            || ip.is_loopback()
            || ip.is_link_local()
            || (a == 192 && b == 0 && c == 0 && d != 9 && d != 10)
            || ip.is_documentation()
            || (a == 198 && matches!(b, 18 | 19))
            || (a & 240) == 240
            || ip.is_broadcast()
            || ip.is_multicast())
    }

    /// Returns whether an `IPv6` address is suitable for a public unicast TCP probe.
    pub fn is_global_ipv6(ip: Ipv6Addr) -> bool {
        let segments = ip.segments();
        let value = u128::from_be_bytes(ip.octets());
        let ietf_protocol_assignment = matches!(segments, [0x2001, b, ..] if b < 0x200)
            && !(value == 0x2001_0001_0000_0000_0000_0000_0000_0001
                || value == 0x2001_0001_0000_0000_0000_0000_0000_0002
                || matches!(segments, [0x2001, 3, ..])
                || matches!(segments, [0x2001, 4, 0x112, ..])
                || matches!(segments, [0x2001, b, ..] if (0x20..=0x3f).contains(&b)));
        let documentation = matches!(segments, [0x2001, 0xdb8, ..] | [0x3fff, 0x0000..=0x0fff, ..]);

        !(ip.is_unspecified()
            || ip.is_loopback()
            || matches!(segments, [0, 0, 0, 0, 0, 0xffff, _, _])
            || matches!(segments, [0x64, 0xff9b, 1, ..])
            || matches!(segments, [0x100, 0, 0, 0, ..])
            || ietf_protocol_assignment
            || matches!(segments, [0x2002, ..])
            || documentation
            || matches!(segments, [0x5f00, ..])
            || ip.is_unique_local()
            || ip.is_unicast_link_local()
            || ip.is_multicast())
    }
}

/// In-memory global and per-source concurrency limiter for probes.
#[derive(Debug)]
pub struct ProbeLimiter {
    global: Arc<Semaphore>,
    active_sources: Arc<Mutex<HashSet<IpAddr>>>,
}

impl ProbeLimiter {
    /// Creates a limiter with the supplied global capacity and one probe per source.
    pub fn new(global_capacity: usize) -> Self {
        Self {
            global: Arc::new(Semaphore::new(global_capacity)),
            active_sources: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Attempts to reserve global and per-source probe capacity.
    pub fn try_acquire(&self, source_ip: IpAddr) -> Option<ProbePermit> {
        let mut active_sources = self.active_sources.lock().unwrap_or_else(PoisonError::into_inner);
        if active_sources.contains(&source_ip) {
            return None;
        }
        let global_permit = Arc::clone(&self.global).try_acquire_owned().ok()?;
        active_sources.insert(source_ip);
        drop(active_sources);

        Some(ProbePermit {
            source_ip,
            _global_permit: global_permit,
            active_sources: Arc::clone(&self.active_sources),
        })
    }
}

/// Capacity reservation held for the lifetime of one probe.
#[derive(Debug)]
pub struct ProbePermit {
    source_ip: IpAddr,
    _global_permit: OwnedSemaphorePermit,
    active_sources: Arc<Mutex<HashSet<IpAddr>>>,
}

impl Drop for ProbePermit {
    fn drop(&mut self) {
        self.active_sources.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.source_ip);
    }
}

/// State shared by execution-layer reachability handlers.
#[derive(Debug, Clone)]
pub struct P2pState {
    resolver: ClientIpResolver,
    limiter: Arc<ProbeLimiter>,
    prober: Arc<dyn ReachabilityProber>,
}

/// Axum routes for execution-layer P2P reachability checks.
#[derive(Debug, Clone, Copy, Default)]
pub struct P2pRoutes;

impl P2pRoutes {
    /// Returns a reachability router using an injected prober.
    pub fn router_with_prober<P>(trusted_proxy_cidrs: Vec<IpNet>, prober: Arc<P>) -> Router
    where
        P: ReachabilityProber + 'static,
    {
        let state = P2pState {
            resolver: ClientIpResolver::new(trusted_proxy_cidrs),
            limiter: Arc::new(ProbeLimiter::new(P2P_REACHABILITY_MAX_CONCURRENT_PROBES)),
            prober,
        };
        Router::new()
            .route(P2P_REACHABILITY_PATH, post(Self::check))
            .layer(DefaultBodyLimit::max(P2P_REACHABILITY_MAX_REQUEST_BYTES))
            .with_state(state)
    }

    /// Handles one execution-layer P2P reachability check.
    pub async fn check(
        State(state): State<P2pState>,
        ConnectInfo(socket_peer): ConnectInfo<SocketAddr>,
        headers: HeaderMap,
        body: Result<Json<P2pReachabilityRequest>, JsonRejection>,
    ) -> Result<Json<P2pReachabilityResponse>, P2pApiError> {
        let source_ip = state.resolver.resolve(socket_peer, &headers).inspect_err(|error| {
            debug!(error = %error, "reachability request source rejected");
        })?;
        let Json(request) = body.map_err(|rejection| {
            debug!(status = %rejection.status(), "reachability request body rejected");
            P2pApiError::from_json_rejection(rejection)
        })?;
        let target = request.target(source_ip).ok_or_else(|| {
            debug!("reachability request target validation failed");
            P2pApiError::InvalidRequest
        })?;
        let _permit = state.limiter.try_acquire(source_ip).ok_or_else(|| {
            debug!("reachability probe capacity exhausted");
            P2pApiError::Saturated
        })?;
        let result = state.prober.probe(target).await;
        let elapsed_ms = u64::try_from(result.elapsed.as_millis()).unwrap_or(u64::MAX);

        info!(
            outcome = ?result.outcome,
            stage = ?result.stage,
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
        net::{IpAddr, Ipv4Addr, SocketAddr},
        str::FromStr,
        sync::{Arc, Mutex, PoisonError},
        time::Duration,
    };

    use alloy_primitives::B512;
    use async_trait::async_trait;
    use axum::{
        Router,
        http::{HeaderMap, HeaderValue, StatusCode},
    };
    use ipnet::IpNet;
    use tokio::{net::TcpListener, sync::Semaphore, task::JoinHandle};

    use super::{
        ClientIpError, ClientIpResolver, P2P_REACHABILITY_PATH, P2pAddressFamily, P2pErrorResponse,
        P2pReachabilityRequest, P2pReachabilityResponse, P2pRoutes, ProbeLimiter, TEST_NODE_ID,
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
            axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .unwrap();
        });
        (address, handle)
    }

    fn trusted_loopback() -> Vec<IpNet> {
        vec![IpNet::from_str("127.0.0.1/32").unwrap()]
    }

    #[test]
    fn resolves_direct_global_client() {
        let resolver = ClientIpResolver::new(Vec::new());
        let resolved =
            resolver.resolve(SocketAddr::from(([8, 8, 8, 8], 1234)), &HeaderMap::new()).unwrap();
        assert_eq!(resolved, IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn walks_forwarded_chain_from_right_to_left() {
        let resolver = ClientIpResolver::new(vec![IpNet::from_str("10.0.0.0/8").unwrap()]);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.1.1.1, 9.9.9.9, 10.0.0.3"));

        let resolved = resolver.resolve(SocketAddr::from(([10, 0, 0, 2], 443)), &headers).unwrap();

        assert_eq!(resolved, IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)));
    }

    #[test]
    fn rejects_forwarding_header_from_untrusted_peer() {
        let resolver = ClientIpResolver::new(Vec::new());
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("8.8.8.8"));

        let error = resolver.resolve(SocketAddr::from(([1, 1, 1, 1], 443)), &headers).unwrap_err();

        assert_eq!(error, ClientIpError::UntrustedForwardedFor);
    }

    #[test]
    fn rejects_trusted_proxy_without_forwarding_header() {
        let resolver = ClientIpResolver::new(vec![IpNet::from_str("10.0.0.0/8").unwrap()]);

        let error = resolver
            .resolve(SocketAddr::from(([10, 0, 0, 2], 443)), &HeaderMap::new())
            .unwrap_err();

        assert_eq!(error, ClientIpError::MissingForwardedFor);
    }

    #[test]
    fn rejects_malformed_forwarding_chain() {
        let resolver = ClientIpResolver::new(vec![IpNet::from_str("10.0.0.0/8").unwrap()]);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("8.8.8.8,not-an-ip"));

        let error = resolver.resolve(SocketAddr::from(([10, 0, 0, 2], 443)), &headers).unwrap_err();

        assert_eq!(error, ClientIpError::MalformedForwardedFor);
    }

    #[test]
    fn rejects_forwarding_chain_over_hop_limit() {
        let resolver = ClientIpResolver::new(vec![IpNet::from_str("10.0.0.0/8").unwrap()]);
        let forwarded = vec!["8.8.8.8"; ClientIpResolver::MAX_FORWARDED_HOPS + 1].join(",");
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_str(&forwarded).unwrap());

        let error = resolver.resolve(SocketAddr::from(([10, 0, 0, 2], 443)), &headers).unwrap_err();

        assert_eq!(error, ClientIpError::MalformedForwardedFor);
    }

    #[test]
    fn rejects_forwarding_chain_with_only_trusted_hops() {
        let resolver = ClientIpResolver::new(vec![IpNet::from_str("10.0.0.0/8").unwrap()]);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("10.0.0.3"));

        let error = resolver.resolve(SocketAddr::from(([10, 0, 0, 2], 443)), &headers).unwrap_err();

        assert_eq!(error, ClientIpError::AllHopsTrusted);
    }

    #[test]
    fn rejects_non_global_direct_client() {
        let resolver = ClientIpResolver::new(Vec::new());

        let error = resolver
            .resolve(SocketAddr::from(([127, 0, 0, 1], 1234)), &HeaderMap::new())
            .unwrap_err();

        assert_eq!(error, ClientIpError::NonGlobalClientIp);
    }

    #[test]
    fn rejects_non_public_special_use_ranges() {
        assert!(!ClientIpResolver::is_global(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(!ClientIpResolver::is_global(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1))));
        assert!(!ClientIpResolver::is_global(IpAddr::from_str("2001:db8::1").unwrap()));
        assert!(!ClientIpResolver::is_global(IpAddr::from_str("ff02::1").unwrap()));
    }

    #[test]
    fn validates_node_identity_and_port() {
        let valid = P2pReachabilityRequest {
            node_id: TEST_NODE_ID.to_string(),
            tcp_port: 30303,
            address_family: P2pAddressFamily::Ipv4,
        };
        assert!(valid.target(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))).is_some());

        let zero_port = P2pReachabilityRequest {
            node_id: valid.node_id,
            tcp_port: 0,
            address_family: valid.address_family,
        };
        assert!(zero_port.target(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))).is_none());

        let invalid_identity = P2pReachabilityRequest {
            node_id: "00".repeat(64),
            tcp_port: 30303,
            address_family: P2pAddressFamily::Ipv4,
        };
        assert!(invalid_identity.target(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))).is_none());

        let wrong_family = P2pReachabilityRequest {
            node_id: TEST_NODE_ID.to_string(),
            tcp_port: 30303,
            address_family: P2pAddressFamily::Ipv6,
        };
        assert!(wrong_family.target(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))).is_none());
    }

    #[test]
    fn duplicate_source_does_not_consume_global_capacity() {
        let limiter = ProbeLimiter::new(2);
        let source = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
        let other = IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9));
        let _source_permit = limiter.try_acquire(source).unwrap();

        assert!(limiter.try_acquire(source).is_none());
        assert!(limiter.try_acquire(other).is_some());
    }

    #[test]
    fn global_rejection_does_not_reserve_source() {
        let limiter = ProbeLimiter::new(1);
        let first = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
        let waiting = IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9));
        let first_permit = limiter.try_acquire(first).unwrap();

        assert!(limiter.try_acquire(waiting).is_none());
        drop(first_permit);
        assert!(limiter.try_acquire(waiting).is_some());
    }

    #[tokio::test]
    async fn returns_completed_probe_as_json() {
        let prober = Arc::new(FakeProber::default());
        let router = P2pRoutes::router_with_prober(trusted_loopback(), Arc::clone(&prober));
        let (address, handle) = start_test_server(router).await;
        let request = P2pReachabilityRequest {
            node_id: TEST_NODE_ID.to_string(),
            tcp_port: 30303,
            address_family: P2pAddressFamily::Ipv4,
        };

        let response = reqwest::Client::new()
            .post(format!("http://{address}{P2P_REACHABILITY_PATH}"))
            .header("x-forwarded-for", "8.8.8.8")
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
                node_id: B512::from_str(request.node_id.trim_start_matches("0x")).unwrap(),
            }]
        );

        handle.abort();
    }

    #[tokio::test]
    async fn rejects_spoofed_forwarding_header() {
        let router = P2pRoutes::router_with_prober(Vec::new(), Arc::new(FakeProber::default()));
        let (address, handle) = start_test_server(router).await;
        let request = P2pReachabilityRequest {
            node_id: TEST_NODE_ID.to_string(),
            tcp_port: 30303,
            address_family: P2pAddressFamily::Ipv4,
        };

        let response = reqwest::Client::new()
            .post(format!("http://{address}{P2P_REACHABILITY_PATH}"))
            .header("x-forwarded-for", "8.8.8.8")
            .json(&request)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error = response.json::<P2pErrorResponse>().await.unwrap();
        assert_eq!(error.error, super::P2pApiError::InvalidSource);
        handle.abort();
    }

    #[tokio::test]
    async fn rejects_oversized_body() {
        let router =
            P2pRoutes::router_with_prober(trusted_loopback(), Arc::new(FakeProber::default()));
        let (address, handle) = start_test_server(router).await;

        let response = reqwest::Client::new()
            .post(format!("http://{address}{P2P_REACHABILITY_PATH}"))
            .header("x-forwarded-for", "8.8.8.8")
            .header("content-type", "application/json")
            .body("x".repeat(2048))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        handle.abort();
    }

    #[tokio::test]
    async fn rejects_second_probe_from_same_source() {
        let prober = Arc::new(BlockingProber {
            entered: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
        });
        let router = P2pRoutes::router_with_prober(trusted_loopback(), Arc::clone(&prober));
        let (address, handle) = start_test_server(router).await;
        let request = P2pReachabilityRequest {
            node_id: TEST_NODE_ID.to_string(),
            tcp_port: 30303,
            address_family: P2pAddressFamily::Ipv4,
        };
        let client = reqwest::Client::new();
        let first_client = client.clone();
        let first_request = request.clone();

        let first = tokio::spawn(async move {
            first_client
                .post(format!("http://{address}{P2P_REACHABILITY_PATH}"))
                .header("x-forwarded-for", "8.8.8.8")
                .json(&first_request)
                .send()
                .await
                .unwrap()
        });
        prober.entered.acquire().await.unwrap().forget();

        let second = client
            .post(format!("http://{address}{P2P_REACHABILITY_PATH}"))
            .header("x-forwarded-for", "8.8.8.8")
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
