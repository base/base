//! HTTP API for execution-layer and consensus-layer P2P reachability checks:
//! request validation and probe concurrency limits.

use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use discv5::enr::{CombinedPublicKey, Enr, EnrPublicKey};
use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
use reth_network_peers::{NodeRecord, id2pk};
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, info};

use crate::{
    cl_prober::{ClReachabilityProber, Libp2pProbeOutcome, Libp2pProbeStage, Libp2pProbeTarget},
    el_prober::{ReachabilityProber, RlpxProbeOutcome, RlpxProbeStage, RlpxProbeTarget},
};

/// HTTP path for execution-layer P2P reachability checks.
pub const P2P_REACHABILITY_EL_PATH: &str = "/v1/p2p/reachability/el";
/// HTTP path for consensus-layer P2P reachability checks.
pub const P2P_REACHABILITY_CL_PATH: &str = "/v1/p2p/reachability/cl";
/// Maximum JSON request body size accepted by the reachability routes.
pub const P2P_REACHABILITY_MAX_REQUEST_BYTES: usize = 1024;
/// Default maximum number of reachability probes allowed in flight globally.
pub const DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES: usize = 32;
/// Valid execution-layer node identity shared by reachability tests.
#[cfg(test)]
pub const TEST_NODE_ID: &str = "2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4";

/// JSON request for an execution-layer reachability check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElReachabilityRequest {
    /// The node's advertised `enode://` URL, as printed on startup and
    /// returned by `admin_nodeInfo`.
    pub enode: String,
}

impl ElReachabilityRequest {
    /// Validates the request and returns its advertised `RLPx` target.
    pub fn target(&self) -> Option<RlpxProbeTarget> {
        if !self.enode.starts_with("enode://") {
            return None;
        }
        let record = NodeRecord::from_str(&self.enode).ok()?;
        id2pk(record.id).ok()?;
        let address = record.tcp_addr();
        (address.port() != 0 && Self::is_public_ip(address.ip()))
            .then_some(RlpxProbeTarget { address, node_id: record.id })
    }

    /// Returns whether `ip` is a supported, publicly routable `IPv4` address.
    /// `IPv6` and non-public targets are rejected because the execution P2P
    /// network currently supports only public `IPv4` reachability probes.
    pub const fn is_public_ip(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => {
                !(v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_documentation()
                    || v4.is_multicast()
                    // "This network" (0.0.0.0/8), including the unspecified address.
                    || v4.octets()[0] == 0
                    // Carrier-grade NAT (100.64.0.0/10).
                    || matches!(v4.octets(), [100, 64..=127, _, _])
                    // Benchmarking (198.18.0.0/15).
                    || matches!(v4.octets(), [198, 18..=19, _, _])
                    // IETF Protocol Assignments (192.0.0.0/24).
                    || matches!(v4.octets(), [192, 0, 0, _])
                    // Deprecated 6to4 relay anycast (192.88.99.0/24).
                    || matches!(v4.octets(), [192, 88, 99, _])
                    // Reserved (240.0.0.0/4), including broadcast.
                    || v4.octets()[0] >= 240)
            }
            IpAddr::V6(_) => false,
        }
    }
}

/// JSON response for a completed execution-layer reachability check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElReachabilityResponse {
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

/// JSON request for a consensus-layer reachability check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClReachabilityRequest {
    /// The node's advertised `enr:` record, as returned by `opp2p_self`, or a
    /// public `IPv4` `/ip4/.../tcp/.../p2p/<peer-id>` multiaddr.
    #[serde(alias = "multiaddr")]
    pub enr: String,
}

impl ClReachabilityRequest {
    /// Validates the request and returns its advertised libp2p target.
    pub fn target(&self) -> Option<Libp2pProbeTarget> {
        if self.enr.starts_with("enr:") {
            Self::enr_target(&self.enr)
        } else {
            Self::multiaddr_target(&self.enr)
        }
    }

    /// Parses a signed `enr:` record into a public `IPv4` libp2p probe target.
    fn enr_target(enr: &str) -> Option<Libp2pProbeTarget> {
        let enr: Enr<discv5::enr::CombinedKey> = enr.parse().ok()?;
        let socket = enr.tcp4_socket()?;
        let address = SocketAddr::from((*socket.ip(), socket.port()));
        if socket.port() == 0 || !ElReachabilityRequest::is_public_ip(address.ip()) {
            return None;
        }
        let CombinedPublicKey::Secp256k1(public_key) = enr.public_key() else {
            return None;
        };
        let public_key =
            libp2p_identity::secp256k1::PublicKey::try_from_bytes(&public_key.encode()).ok()?;
        let peer_id = PeerId::from_public_key(&public_key.into());
        Some(Libp2pProbeTarget { address, peer_id })
    }

    /// Parses a `/ip4/<addr>/tcp/<port>/p2p/<peer-id>` multiaddr into a public
    /// `IPv4` libp2p probe target. Extra protocols, `IPv6`, and non-public IPs are
    /// rejected.
    fn multiaddr_target(value: &str) -> Option<Libp2pProbeTarget> {
        let addr: Multiaddr = value.parse().ok()?;
        let mut protocols = addr.iter();
        let ip = match protocols.next()? {
            Protocol::Ip4(ip) => ip,
            _ => return None,
        };
        let port = match protocols.next()? {
            Protocol::Tcp(port) => port,
            _ => return None,
        };
        let peer_id = match protocols.next()? {
            Protocol::P2p(peer_id) => peer_id,
            _ => return None,
        };
        if protocols.next().is_some() || port == 0 {
            return None;
        }
        let address = SocketAddr::from((ip, port));
        ElReachabilityRequest::is_public_ip(address.ip())
            .then_some(Libp2pProbeTarget { address, peer_id })
    }
}

/// JSON response for a completed consensus-layer reachability check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClReachabilityResponse {
    /// Stable outcome of the probe.
    pub outcome: Libp2pProbeOutcome,
    /// Protocol stage reached by the probe.
    pub stage: Libp2pProbeStage,
    /// Advertised address probed by the service.
    pub observed_address: SocketAddr,
    /// Total probe duration in milliseconds.
    pub elapsed_ms: u64,
    /// Agent version returned by the remote libp2p identify exchange.
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

/// State shared by the reachability handlers.
///
/// Both layers draw probes from one global capacity semaphore: the limit
/// protects the probing host, not any single target network.
#[derive(Debug, Clone)]
pub struct P2pState {
    limiter: Arc<Semaphore>,
    el_prober: Arc<dyn ReachabilityProber>,
    cl_prober: Arc<dyn ClReachabilityProber>,
}

impl P2pState {
    /// Creates handler state with the supplied global probe capacity.
    pub fn new<El, Cl>(global_capacity: usize, el_prober: Arc<El>, cl_prober: Arc<Cl>) -> Self
    where
        El: ReachabilityProber + 'static,
        Cl: ClReachabilityProber + 'static,
    {
        Self { limiter: Arc::new(Semaphore::new(global_capacity)), el_prober, cl_prober }
    }
}

/// Axum routes for P2P reachability checks.
#[derive(Debug, Clone, Copy, Default)]
pub struct P2pRoutes;

impl P2pRoutes {
    /// Returns a reachability router using injected probers.
    pub fn router_with_probers<El, Cl>(
        global_capacity: usize,
        el_prober: Arc<El>,
        cl_prober: Arc<Cl>,
    ) -> Router
    where
        El: ReachabilityProber + 'static,
        Cl: ClReachabilityProber + 'static,
    {
        Router::new()
            .route(P2P_REACHABILITY_EL_PATH, post(Self::check_el))
            .route(P2P_REACHABILITY_CL_PATH, post(Self::check_cl))
            .layer(DefaultBodyLimit::max(P2P_REACHABILITY_MAX_REQUEST_BYTES))
            .with_state(P2pState::new(global_capacity, el_prober, cl_prober))
    }

    /// Validates one reachability request body, resolves its probe target,
    /// and reserves global probe capacity. Shared by both layer handlers.
    fn accept_probe<R, T>(
        state: &P2pState,
        layer: &'static str,
        body: Result<Json<R>, JsonRejection>,
        target: fn(&R) -> Option<T>,
    ) -> Result<(T, OwnedSemaphorePermit), P2pApiError> {
        let Json(request) = body.map_err(|rejection| {
            debug!(status = %rejection.status(), layer, "reachability request body rejected");
            P2pApiError::from_json_rejection(rejection)
        })?;
        let target = target(&request).ok_or_else(|| {
            debug!(layer, "reachability request target validation failed");
            P2pApiError::InvalidRequest
        })?;
        let permit = Arc::clone(&state.limiter).try_acquire_owned().map_err(|_| {
            debug!(layer, "reachability probe capacity exhausted");
            P2pApiError::Saturated
        })?;
        Ok((target, permit))
    }

    /// Handles one execution-layer P2P reachability check.
    pub async fn check_el(
        State(state): State<P2pState>,
        body: Result<Json<ElReachabilityRequest>, JsonRejection>,
    ) -> Result<Json<ElReachabilityResponse>, P2pApiError> {
        let (target, _permit) =
            Self::accept_probe(&state, "el", body, ElReachabilityRequest::target)?;
        let result = state.el_prober.probe(target).await;
        let elapsed_ms = u64::try_from(result.elapsed.as_millis()).unwrap_or(u64::MAX);

        info!(
            outcome = %result.outcome,
            stage = %result.stage,
            target = %target.address,
            elapsed_ms,
            layer = "el",
            "reachability probe completed"
        );

        Ok(Json(ElReachabilityResponse {
            outcome: result.outcome,
            stage: result.stage,
            observed_address: target.address,
            elapsed_ms,
            client_version: result.client_version,
        }))
    }

    /// Handles one consensus-layer P2P reachability check.
    pub async fn check_cl(
        State(state): State<P2pState>,
        body: Result<Json<ClReachabilityRequest>, JsonRejection>,
    ) -> Result<Json<ClReachabilityResponse>, P2pApiError> {
        let (target, _permit) =
            Self::accept_probe(&state, "cl", body, ClReachabilityRequest::target)?;
        let result = state.cl_prober.probe(target).await;
        let elapsed_ms = u64::try_from(result.elapsed.as_millis()).unwrap_or(u64::MAX);

        info!(
            outcome = %result.outcome,
            stage = %result.stage,
            target = %target.address,
            elapsed_ms,
            layer = "cl",
            "reachability probe completed"
        );

        Ok(Json(ClReachabilityResponse {
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
    use std::{net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

    use alloy_primitives::B512;
    use axum::{Router, http::StatusCode};
    use discv5::enr::{CombinedKey, Enr};
    use tokio::{net::TcpListener, sync::Semaphore, task::JoinHandle};

    use super::{
        ClReachabilityRequest, ClReachabilityResponse,
        DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES, ElReachabilityRequest,
        ElReachabilityResponse, P2P_REACHABILITY_CL_PATH, P2P_REACHABILITY_EL_PATH, P2pRoutes,
        TEST_NODE_ID,
    };
    use crate::{
        BlockingProber, Libp2pProbeOutcome, Libp2pProbeResult, Libp2pProbeStage,
        MockClReachabilityProber, MockReachabilityProber, RlpxProbeOutcome, RlpxProbeResult,
        RlpxProbeStage, RlpxProbeTarget,
    };

    async fn start_test_server(router: Router) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (address, handle)
    }

    fn router_with_el_prober(
        global_capacity: usize,
        prober: Arc<MockReachabilityProber>,
    ) -> Router {
        // No CL expectations: any CL probe panics and fails its test.
        P2pRoutes::router_with_probers(
            global_capacity,
            prober,
            Arc::new(MockClReachabilityProber::new()),
        )
    }

    fn router_with_cl_prober(
        global_capacity: usize,
        prober: Arc<MockClReachabilityProber>,
    ) -> Router {
        // No EL expectations: any EL probe panics and fails its test.
        P2pRoutes::router_with_probers(
            global_capacity,
            Arc::new(MockReachabilityProber::new()),
            prober,
        )
    }

    fn test_request() -> ElReachabilityRequest {
        ElReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@8.8.8.8:30303") }
    }

    /// Builds a signed test ENR advertising `ip:tcp_port`.
    fn test_enr(ip: [u8; 4], tcp_port: u16) -> String {
        let key = CombinedKey::generate_secp256k1();
        Enr::builder().ip4(ip.into()).tcp4(tcp_port).build(&key).unwrap().to_base64()
    }

    fn test_cl_request() -> ClReachabilityRequest {
        ClReachabilityRequest { enr: test_enr([8, 8, 8, 8], 9222) }
    }

    fn test_peer_id() -> libp2p::PeerId {
        libp2p::identity::Keypair::generate_secp256k1().public().to_peer_id()
    }

    fn test_multiaddr(ip: [u8; 4], port: u16, peer_id: libp2p::PeerId) -> String {
        format!("/ip4/{}.{}.{}.{}/tcp/{port}/p2p/{peer_id}", ip[0], ip[1], ip[2], ip[3])
    }

    #[test]
    fn validates_enode_identity_address_and_port() {
        assert_eq!(
            test_request().target().unwrap().address,
            SocketAddr::from(([8, 8, 8, 8], 30303))
        );

        let with_discport = ElReachabilityRequest {
            enode: format!("enode://{TEST_NODE_ID}@8.8.8.8:30303?discport=30301"),
        };
        assert_eq!(
            with_discport.target().unwrap().address,
            SocketAddr::from(([8, 8, 8, 8], 30303))
        );

        let invalid_discport = ElReachabilityRequest {
            enode: format!("enode://{TEST_NODE_ID}@8.8.8.8:30303?discport=invalid"),
        };
        assert!(invalid_discport.target().is_none());

        let zero_port =
            ElReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@8.8.8.8:0") };
        assert!(zero_port.target().is_none());

        let invalid_identity =
            ElReachabilityRequest { enode: format!("enode://{}@8.8.8.8:30303", "00".repeat(64)) };
        assert!(invalid_identity.target().is_none());

        let missing_scheme =
            ElReachabilityRequest { enode: format!("{TEST_NODE_ID}@8.8.8.8:30303") };
        assert!(missing_scheme.target().is_none());

        let hostname =
            ElReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@example.com:30303") };
        assert!(hostname.target().is_none());

        // Non-public targets are rejected to keep untrusted enodes from
        // steering probes at internal networks.
        for ip in [
            "10.0.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "0.1.2.3",
            "198.18.0.1",
            "198.19.255.255",
            "192.0.0.8",
            "192.88.99.1",
            "240.0.0.1",
            "255.255.255.255",
            "[2606:4700:4700::1111]",
            "[::1]",
            "[fc00::1]",
            "[fe80::1]",
            "[::ffff:10.0.0.1]",
            "[64:ff9b::a00:1]",
            "[2002:a00:1::]",
            "[::a00:1]",
            "[::]",
        ] {
            let non_public =
                ElReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@{ip}:30303") };
            assert!(non_public.target().is_none(), "expected {ip} to be rejected");
        }
    }

    #[test]
    fn probes_advertised_enode_address() {
        let request =
            ElReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@9.9.9.9:30303") };

        let target = request.target().unwrap();

        assert_eq!(target.address, SocketAddr::from(([9, 9, 9, 9], 30303)));
    }

    #[tokio::test]
    async fn ignores_forwarded_header_and_probes_enode_address() {
        // A probe against any other target panics with no matching expectation.
        let expected = RlpxProbeTarget {
            address: SocketAddr::from(([8, 8, 8, 8], 30303)),
            node_id: B512::from_str(TEST_NODE_ID).unwrap(),
        };
        let mut prober = MockReachabilityProber::new();
        prober.expect_probe().times(1).withf(move |target| *target == expected).returning(|_| {
            RlpxProbeResult {
                outcome: RlpxProbeOutcome::Reachable,
                stage: RlpxProbeStage::Devp2pHello,
                elapsed: Duration::from_millis(12),
                client_version: Some("test-peer/1.0".to_string()),
            }
        });
        let router =
            router_with_el_prober(DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES, Arc::new(prober));
        let (address, handle) = start_test_server(router).await;
        let request = test_request();

        let response = reqwest::Client::new()
            .post(format!("http://{address}{P2P_REACHABILITY_EL_PATH}"))
            .header("x-forwarded-for", "1.1.1.1")
            .json(&request)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let response = response.json::<ElReachabilityResponse>().await.unwrap();
        assert_eq!(response.outcome, RlpxProbeOutcome::Reachable);
        assert_eq!(response.observed_address, SocketAddr::from(([8, 8, 8, 8], 30303)));
        assert_eq!(response.client_version.as_deref(), Some("test-peer/1.0"));

        handle.abort();
    }

    #[tokio::test]
    async fn rejects_private_target() {
        // No expectations: any probe call panics and fails the status assert.
        let router = router_with_el_prober(
            DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            Arc::new(MockReachabilityProber::new()),
        );
        let (address, handle) = start_test_server(router).await;
        let request =
            ElReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@10.0.0.1:30303") };

        let response = reqwest::Client::new()
            .post(format!("http://{address}{P2P_REACHABILITY_EL_PATH}"))
            .json(&request)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        handle.abort();
    }

    #[tokio::test]
    async fn rejects_oversized_body() {
        let router = router_with_el_prober(
            DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            Arc::new(MockReachabilityProber::new()),
        );
        let (address, handle) = start_test_server(router).await;

        let response = reqwest::Client::new()
            .post(format!("http://{address}{P2P_REACHABILITY_EL_PATH}"))
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
        let router = P2pRoutes::router_with_probers(
            1,
            Arc::clone(&prober),
            Arc::new(MockClReachabilityProber::new()),
        );
        let (address, handle) = start_test_server(router).await;
        let request = test_request();
        let client = reqwest::Client::new();
        let first_client = client.clone();
        let first_request = request.clone();

        let first = tokio::spawn(async move {
            first_client
                .post(format!("http://{address}{P2P_REACHABILITY_EL_PATH}"))
                .json(&first_request)
                .send()
                .await
                .unwrap()
        });
        prober.entered.acquire().await.unwrap().forget();

        let second = client
            .post(format!("http://{address}{P2P_REACHABILITY_EL_PATH}"))
            .json(&request)
            .send()
            .await
            .unwrap();

        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        prober.release.add_permits(1);
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
        handle.abort();
    }

    #[test]
    fn validates_enr_identity_address_and_port() {
        let valid = ClReachabilityRequest { enr: test_enr([8, 8, 8, 8], 9222) };
        let target = valid.target().unwrap();
        assert_eq!(target.address, SocketAddr::from(([8, 8, 8, 8], 9222)));

        let missing_prefix =
            ClReachabilityRequest { enr: valid.enr.strip_prefix("enr:").unwrap().to_string() };
        assert!(missing_prefix.target().is_none());

        let garbage = ClReachabilityRequest { enr: "enr:not-a-record".to_string() };
        assert!(garbage.target().is_none());

        let zero_port = ClReachabilityRequest { enr: test_enr([8, 8, 8, 8], 0) };
        assert!(zero_port.target().is_none());

        let key = CombinedKey::generate_secp256k1();
        let no_tcp = Enr::builder().ip4([8, 8, 8, 8].into()).udp4(9222).build(&key).unwrap();
        let no_tcp = ClReachabilityRequest { enr: no_tcp.to_base64() };
        assert!(no_tcp.target().is_none());

        // Non-public targets are rejected to keep untrusted ENRs from
        // steering probes at internal networks.
        for ip in [
            [10, 0, 0, 1],
            [127, 0, 0, 1],
            [169, 254, 169, 254],
            [100, 64, 0, 1],
            [0, 0, 0, 0],
            [198, 18, 0, 1],
            [192, 0, 0, 8],
            [240, 0, 0, 1],
        ] {
            let non_public = ClReachabilityRequest { enr: test_enr(ip, 9222) };
            assert!(non_public.target().is_none(), "expected {ip:?} to be rejected");
        }
    }

    #[test]
    fn derives_peer_id_from_enr_key() {
        let key = CombinedKey::generate_secp256k1();
        let enr = Enr::builder().ip4([8, 8, 8, 8].into()).tcp4(9222).build(&key).unwrap();
        let request = ClReachabilityRequest { enr: enr.to_base64() };

        let target = request.target().unwrap();

        // The libp2p peer ID must be derived from the same secp256k1 key that
        // signed the ENR.
        let discv5::enr::CombinedPublicKey::Secp256k1(public_key) = enr.public_key() else {
            panic!("expected a secp256k1 ENR key");
        };
        let public_key = libp2p_identity::secp256k1::PublicKey::try_from_bytes(
            &discv5::enr::EnrPublicKey::encode(&public_key),
        )
        .unwrap();
        let expected = libp2p::PeerId::from_public_key(&public_key.into());
        assert_eq!(target.peer_id, expected);
    }

    #[test]
    fn validates_multiaddr_identity_address_and_port() {
        let peer_id = test_peer_id();
        let valid = ClReachabilityRequest { enr: test_multiaddr([8, 8, 8, 8], 9222, peer_id) };
        let target = valid.target().unwrap();
        assert_eq!(target.address, SocketAddr::from(([8, 8, 8, 8], 9222)));
        assert_eq!(target.peer_id, peer_id);

        let missing_peer_id = ClReachabilityRequest { enr: "/ip4/8.8.8.8/tcp/9222".to_string() };
        assert!(missing_peer_id.target().is_none());

        let udp_only =
            ClReachabilityRequest { enr: format!("/ip4/8.8.8.8/udp/9222/p2p/{peer_id}") };
        assert!(udp_only.target().is_none());

        let ipv6 =
            ClReachabilityRequest { enr: format!("/ip6/2001:db8::1/tcp/9222/p2p/{peer_id}") };
        assert!(ipv6.target().is_none());

        let extra_protocol =
            ClReachabilityRequest { enr: format!("/ip4/8.8.8.8/tcp/9222/ws/p2p/{peer_id}") };
        assert!(extra_protocol.target().is_none());

        let zero_port = ClReachabilityRequest { enr: test_multiaddr([8, 8, 8, 8], 0, peer_id) };
        assert!(zero_port.target().is_none());

        let garbage = ClReachabilityRequest { enr: "/ip4/not-an-ip/tcp/9222/p2p/x".to_string() };
        assert!(garbage.target().is_none());

        for ip in [
            [10, 0, 0, 1],
            [127, 0, 0, 1],
            [169, 254, 169, 254],
            [100, 64, 0, 1],
            [0, 0, 0, 0],
            [198, 18, 0, 1],
            [192, 0, 0, 8],
            [240, 0, 0, 1],
        ] {
            let non_public = ClReachabilityRequest { enr: test_multiaddr(ip, 9222, peer_id) };
            assert!(non_public.target().is_none(), "expected {ip:?} to be rejected");
        }
    }

    #[tokio::test]
    async fn probes_advertised_enr_address() {
        let request = test_cl_request();
        let expected = request.target().unwrap();
        let mut prober = MockClReachabilityProber::new();
        prober.expect_probe().times(1).withf(move |target| *target == expected).returning(|_| {
            Libp2pProbeResult {
                outcome: Libp2pProbeOutcome::Reachable,
                stage: Libp2pProbeStage::Identify,
                elapsed: Duration::from_millis(12),
                client_version: Some("base".to_string()),
            }
        });
        let router =
            router_with_cl_prober(DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES, Arc::new(prober));
        let (address, handle) = start_test_server(router).await;

        let response = reqwest::Client::new()
            .post(format!("http://{address}{P2P_REACHABILITY_CL_PATH}"))
            .json(&request)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let response = response.json::<ClReachabilityResponse>().await.unwrap();
        assert_eq!(response.outcome, Libp2pProbeOutcome::Reachable);
        assert_eq!(response.observed_address, SocketAddr::from(([8, 8, 8, 8], 9222)));
        assert_eq!(response.client_version.as_deref(), Some("base"));

        handle.abort();
    }

    #[tokio::test]
    async fn probes_advertised_multiaddr() {
        let peer_id = test_peer_id();
        let request = ClReachabilityRequest { enr: test_multiaddr([8, 8, 8, 8], 9222, peer_id) };
        let expected = request.target().unwrap();
        let mut prober = MockClReachabilityProber::new();
        prober.expect_probe().times(1).withf(move |target| *target == expected).returning(|_| {
            Libp2pProbeResult {
                outcome: Libp2pProbeOutcome::Reachable,
                stage: Libp2pProbeStage::Identify,
                elapsed: Duration::from_millis(12),
                client_version: Some("base".to_string()),
            }
        });
        let router =
            router_with_cl_prober(DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES, Arc::new(prober));
        let (address, handle) = start_test_server(router).await;

        let response = reqwest::Client::new()
            .post(format!("http://{address}{P2P_REACHABILITY_CL_PATH}"))
            .json(&std::collections::BTreeMap::from([("multiaddr", request.enr.as_str())]))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let response = response.json::<ClReachabilityResponse>().await.unwrap();
        assert_eq!(response.outcome, Libp2pProbeOutcome::Reachable);
        assert_eq!(response.observed_address, SocketAddr::from(([8, 8, 8, 8], 9222)));
        assert_eq!(response.client_version.as_deref(), Some("base"));

        handle.abort();
    }

    #[tokio::test]
    async fn rejects_private_cl_target() {
        // No expectations: any probe call panics and fails the status assert.
        let router = router_with_cl_prober(
            DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            Arc::new(MockClReachabilityProber::new()),
        );
        let (address, handle) = start_test_server(router).await;
        let request = ClReachabilityRequest { enr: test_enr([10, 0, 0, 1], 9222) };

        let response = reqwest::Client::new()
            .post(format!("http://{address}{P2P_REACHABILITY_CL_PATH}"))
            .json(&request)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        handle.abort();
    }

    #[tokio::test]
    async fn rejects_private_cl_multiaddr() {
        // No expectations: any probe call panics and fails the status assert.
        let router = router_with_cl_prober(
            DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            Arc::new(MockClReachabilityProber::new()),
        );
        let (address, handle) = start_test_server(router).await;
        let request =
            ClReachabilityRequest { enr: test_multiaddr([10, 0, 0, 1], 9222, test_peer_id()) };

        let response = reqwest::Client::new()
            .post(format!("http://{address}{P2P_REACHABILITY_CL_PATH}"))
            .json(&request)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        handle.abort();
    }

    #[tokio::test]
    async fn cl_probe_shares_global_capacity_with_el_probe() {
        // Exhaust the single capacity permit with a parked EL probe, then
        // expect the CL route to report saturation.
        let el_prober = Arc::new(BlockingProber {
            entered: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
        });
        let router = P2pRoutes::router_with_probers(
            1,
            Arc::clone(&el_prober),
            Arc::new(MockClReachabilityProber::new()),
        );
        let (address, handle) = start_test_server(router).await;
        let client = reqwest::Client::new();
        let el_client = client.clone();

        let el_request = tokio::spawn(async move {
            el_client
                .post(format!("http://{address}{P2P_REACHABILITY_EL_PATH}"))
                .json(&test_request())
                .send()
                .await
                .unwrap()
        });
        el_prober.entered.acquire().await.unwrap().forget();

        let cl_response = client
            .post(format!("http://{address}{P2P_REACHABILITY_CL_PATH}"))
            .json(&test_cl_request())
            .send()
            .await
            .unwrap();

        assert_eq!(cl_response.status(), StatusCode::TOO_MANY_REQUESTS);
        el_prober.release.add_permits(1);
        assert_eq!(el_request.await.unwrap().status(), StatusCode::OK);
        handle.abort();
    }
}
