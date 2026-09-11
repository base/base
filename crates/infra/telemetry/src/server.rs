use std::{
    net::SocketAddr,
    num::NonZeroU32,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Context;
use axum::{Router, middleware};
use base_health::HealthServer;
use base_trusted_proxy::TrustedProxyConfig;
use clap::{Args, builder::RangedU64ValueParser};
use ipnet::IpNet;
use tokio::{net::TcpListener, sync::Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    CLIENT_IP_HEADER, ClReachabilityProber, DEFAULT_NODE_REPORT_REQUESTS_PER_HOUR,
    DEFAULT_P2P_PROBE_REQUESTS_PER_MINUTE, DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
    IngestRoutes, IpRateLimiter, JsonlRecorder, Libp2pProber, P2pRoutes, PerIpRateLimit,
    ReachabilityProber, ReportRecorder, RlpxProber,
};

/// Configuration for the Base telemetry HTTP server.
#[derive(Args, Debug, Clone)]
pub struct ServerConfig {
    /// Socket address to bind the HTTP server to.
    #[arg(long, env = "BASE_TELEMETRY_LISTEN_ADDR", default_value = "0.0.0.0:8080")]
    pub listen_addr: SocketAddr,
    /// Comma-separated CIDRs of proxies trusted to supply the `X-Forwarded-For` client IP.
    #[arg(
        long,
        env = "BASE_TELEMETRY_TRUSTED_PROXY_CIDRS",
        value_delimiter = ',',
        value_parser = TrustedProxyConfig::parse_cidr
    )]
    pub trusted_proxy_cidrs: Vec<IpNet>,
    /// P2P reachability probe requests allowed per minute for each client IP.
    #[arg(
        long,
        env = "BASE_TELEMETRY_P2P_PROBE_REQUESTS_PER_MINUTE",
        default_value_t = DEFAULT_P2P_PROBE_REQUESTS_PER_MINUTE
    )]
    pub p2p_probe_requests_per_minute: NonZeroU32,
    /// Maximum number of P2P reachability probes allowed in flight globally.
    #[arg(
        long,
        env = "BASE_TELEMETRY_P2P_MAX_CONCURRENT_PROBES",
        default_value_t = DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
        value_parser = RangedU64ValueParser::<usize>::new().range(1..=Semaphore::MAX_PERMITS as u64)
    )]
    pub p2p_max_concurrent_probes: usize,
    /// Node reports accepted per hour from each client IP.
    #[arg(
        long,
        env = "BASE_TELEMETRY_NODE_REPORT_REQUESTS_PER_HOUR",
        default_value_t = DEFAULT_NODE_REPORT_REQUESTS_PER_HOUR
    )]
    pub node_report_requests_per_hour: NonZeroU32,
    /// File accepted node reports are appended to as JSONL.
    ///
    /// When unset, reports are only emitted as structured log events.
    #[arg(long, env = "BASE_TELEMETRY_NODE_REPORT_PATH")]
    pub node_report_path: Option<PathBuf>,
}

/// Base telemetry Axum server scaffold.
#[derive(Debug, Clone, Copy, Default)]
pub struct BaseTelemetryServer;

impl BaseTelemetryServer {
    /// Returns the application router using injected probers.
    pub fn router_with_probers<El, Cl>(
        ready: Arc<AtomicBool>,
        per_ip: PerIpRateLimit,
        max_concurrent_probes: usize,
        el_prober: Arc<El>,
        cl_prober: Arc<Cl>,
    ) -> Router
    where
        El: ReachabilityProber + 'static,
        Cl: ClReachabilityProber + 'static,
    {
        let p2p = P2pRoutes::router_with_probers(max_concurrent_probes, el_prober, cl_prober)
            .layer(middleware::from_fn_with_state(per_ip, PerIpRateLimit::enforce));
        HealthServer::router(ready).merge(p2p)
    }

    /// Returns the node report ingest router, rate limited on its own quota.
    ///
    /// Ingest is limited separately from the reachability probes: a probe costs an outbound
    /// connection and a report costs an append, so one quota cannot serve both.
    pub fn ingest_router(recorder: Arc<dyn ReportRecorder>, per_ip: PerIpRateLimit) -> Router {
        IngestRoutes::router(recorder, Arc::clone(per_ip.proxy()))
            .layer(middleware::from_fn_with_state(per_ip, PerIpRateLimit::enforce))
    }

    /// Starts the telemetry service with the provided configuration.
    pub async fn serve(config: ServerConfig, cancel: CancellationToken) -> anyhow::Result<()> {
        let listen_addr = config.listen_addr;
        let proxy = Arc::new(TrustedProxyConfig::new(
            CLIENT_IP_HEADER.to_string(),
            config.trusted_proxy_cidrs,
        ));
        let limiter = Arc::new(IpRateLimiter::per_minute(config.p2p_probe_requests_per_minute));
        let eviction = limiter.spawn_eviction_task(cancel.clone());
        let ingest_limiter =
            Arc::new(IpRateLimiter::per_hour(config.node_report_requests_per_hour));
        let ingest_eviction = ingest_limiter.spawn_eviction_task(cancel.clone());

        let recorder = Arc::new(match config.node_report_path {
            Some(path) => JsonlRecorder::new(&path)
                .with_context(|| format!("failed to open node report file {}", path.display()))?,
            None => JsonlRecorder::log_only(),
        });

        let ready = Arc::new(AtomicBool::new(false));
        let app = Self::router_with_probers(
            Arc::clone(&ready),
            PerIpRateLimit::new(limiter, Arc::clone(&proxy)),
            config.p2p_max_concurrent_probes,
            Arc::new(RlpxProber::ephemeral()),
            Arc::new(Libp2pProber::ephemeral()),
        )
        .merge(Self::ingest_router(recorder, PerIpRateLimit::new(ingest_limiter, proxy)));
        let listener = TcpListener::bind(listen_addr)
            .await
            .with_context(|| format!("failed to bind base telemetry server to {listen_addr}"))?;
        let listen_addr =
            listener.local_addr().context("failed to read base telemetry listen address")?;

        ready.store(true, Ordering::SeqCst);

        info!(listen_addr = %listen_addr, "base telemetry server started");

        axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await
            .context("base telemetry server exited unexpectedly")?;

        let _ = eviction.await;
        let _ = ingest_eviction.await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        num::NonZeroU32,
        sync::{Arc, atomic::AtomicBool},
        time::Duration,
    };

    use axum::{Router, http::StatusCode};
    use base_telemetry_types::NodeReport;
    use base_trusted_proxy::TrustedProxyConfig;
    use tokio::{net::TcpListener, sync::Semaphore, task::JoinHandle};

    use crate::{
        BaseTelemetryServer, BlockingProber, DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
        ElReachabilityRequest, IpRateLimiter, JsonlRecorder, MockClReachabilityProber,
        MockReachabilityProber, NODE_REPORT_PATH, P2P_REACHABILITY_EL_PATH, PerIpRateLimit,
        RlpxProbeOutcome, RlpxProbeResult, RlpxProbeStage, TEST_NODE_ID,
    };

    /// Returns a mock prober that answers every probe as reachable.
    fn reachable_prober() -> Arc<MockReachabilityProber> {
        let mut prober = MockReachabilityProber::new();
        prober.expect_probe().returning(|_| RlpxProbeResult {
            outcome: RlpxProbeOutcome::Reachable,
            stage: RlpxProbeStage::Devp2pHello,
            elapsed: Duration::from_millis(1),
            client_version: None,
        });
        Arc::new(prober)
    }

    fn per_ip(per_minute: u32, trusted_proxy_cidrs: Vec<&str>) -> PerIpRateLimit {
        let cidrs = trusted_proxy_cidrs.into_iter().map(|cidr| cidr.parse().unwrap()).collect();
        PerIpRateLimit::new(
            Arc::new(IpRateLimiter::per_minute(NonZeroU32::new(per_minute).unwrap())),
            Arc::new(TrustedProxyConfig::new("x-forwarded-for".to_string(), cidrs)),
        )
    }

    async fn start_router(router: Router) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .unwrap();
        });
        (addr, handle)
    }

    async fn start_test_server() -> (SocketAddr, JoinHandle<()>) {
        start_router(BaseTelemetryServer::router_with_probers(
            Arc::new(AtomicBool::new(true)),
            per_ip(1000, vec![]),
            DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            reachable_prober(),
            Arc::new(MockClReachabilityProber::new()),
        ))
        .await
    }

    fn test_request() -> ElReachabilityRequest {
        ElReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@8.8.8.8:30303") }
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let (addr, handle) = start_test_server().await;

        let response = reqwest::get(format!("http://{addr}/healthz")).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        handle.abort();
    }

    #[tokio::test]
    async fn readyz_returns_ok() {
        let (addr, handle) = start_test_server().await;

        let response = reqwest::get(format!("http://{addr}/readyz")).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        handle.abort();
    }

    #[tokio::test]
    async fn health_routes_remain_available_during_saturated_probe() {
        let prober = Arc::new(BlockingProber {
            entered: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
        });
        let router = BaseTelemetryServer::router_with_probers(
            Arc::new(AtomicBool::new(true)),
            per_ip(1000, vec![]),
            DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            Arc::clone(&prober),
            Arc::new(MockClReachabilityProber::new()),
        );
        let (addr, handle) = start_router(router).await;
        let request = test_request();
        let client = reqwest::Client::new();
        let probe_client = client.clone();

        let probe = tokio::spawn(async move {
            probe_client
                .post(format!("http://{addr}{P2P_REACHABILITY_EL_PATH}"))
                .json(&request)
                .send()
                .await
                .unwrap()
        });
        prober.entered.acquire().await.unwrap().forget();

        let health = client
            .get(format!("http://{addr}/healthz"))
            .body("x".repeat(2048))
            .send()
            .await
            .unwrap();
        let ready = client.get(format!("http://{addr}/readyz")).send().await.unwrap();

        assert_eq!(health.status(), StatusCode::OK);
        assert_eq!(ready.status(), StatusCode::OK);

        prober.release.add_permits(1);
        assert_eq!(probe.await.unwrap().status(), StatusCode::OK);
        handle.abort();
    }

    #[tokio::test]
    async fn rate_limits_requests_per_client_ip() {
        let router = BaseTelemetryServer::router_with_probers(
            Arc::new(AtomicBool::new(true)),
            per_ip(1, vec![]),
            DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            reachable_prober(),
            Arc::new(MockClReachabilityProber::new()),
        );
        let (addr, handle) = start_router(router).await;
        let client = reqwest::Client::new();
        let url = format!("http://{addr}{P2P_REACHABILITY_EL_PATH}");

        let first = client.post(&url).json(&test_request()).send().await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = client.post(&url).json(&test_request()).send().await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(second.headers().contains_key("retry-after"));
        assert_eq!(second.text().await.unwrap(), r#"{"error":"rate_limited"}"#);

        // Health routes are not subject to the per-IP limit.
        let health = client.get(format!("http://{addr}/healthz")).send().await.unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        handle.abort();
    }

    #[tokio::test]
    async fn ingest_is_rate_limited_on_its_own_quota() {
        let router = BaseTelemetryServer::router_with_probers(
            Arc::new(AtomicBool::new(true)),
            per_ip(1, vec![]),
            DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            reachable_prober(),
            Arc::new(MockClReachabilityProber::new()),
        )
        .merge(BaseTelemetryServer::ingest_router(
            Arc::new(JsonlRecorder::log_only()),
            per_ip(1, vec![]),
        ));
        let (addr, handle) = start_router(router).await;
        let client = reqwest::Client::new();
        let ingest_url = format!("http://{addr}{NODE_REPORT_PATH}");
        let report = NodeReport::default();

        let first = client.post(&ingest_url).json(&report).send().await.unwrap();
        assert_eq!(first.status(), StatusCode::ACCEPTED);

        let second = client.post(&ingest_url).json(&report).send().await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(second.headers().contains_key("retry-after"));

        // The probe quota is a separate bucket, so exhausting ingest does not close probing.
        let probe = client
            .post(format!("http://{addr}{P2P_REACHABILITY_EL_PATH}"))
            .json(&test_request())
            .send()
            .await
            .unwrap();
        assert_eq!(probe.status(), StatusCode::OK);

        handle.abort();
    }

    #[tokio::test]
    async fn ignores_forwarded_header_from_untrusted_peer() {
        let router = BaseTelemetryServer::router_with_probers(
            Arc::new(AtomicBool::new(true)),
            per_ip(1, vec![]),
            DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            reachable_prober(),
            Arc::new(MockClReachabilityProber::new()),
        );
        let (addr, handle) = start_router(router).await;
        let client = reqwest::Client::new();
        let url = format!("http://{addr}{P2P_REACHABILITY_EL_PATH}");

        let first = client
            .post(&url)
            .header("x-forwarded-for", "203.0.113.1, 198.51.100.1")
            .json(&test_request())
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        // A spoofed header must not open a fresh rate-limit bucket.
        let second = client
            .post(&url)
            .header("x-forwarded-for", "203.0.113.1, 198.51.100.2")
            .json(&test_request())
            .send()
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);

        handle.abort();
    }

    #[tokio::test]
    async fn honors_forwarded_header_from_trusted_proxy() {
        let router = BaseTelemetryServer::router_with_probers(
            Arc::new(AtomicBool::new(true)),
            per_ip(1, vec!["127.0.0.0/8"]),
            DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            reachable_prober(),
            Arc::new(MockClReachabilityProber::new()),
        );
        let (addr, handle) = start_router(router).await;
        let client = reqwest::Client::new();
        let url = format!("http://{addr}{P2P_REACHABILITY_EL_PATH}");

        let first = client
            .post(&url)
            .header("x-forwarded-for", "198.51.100.1")
            .json(&test_request())
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        // Distinct forwarded clients get independent buckets.
        let second = client
            .post(&url)
            .header("x-forwarded-for", "198.51.100.2")
            .json(&test_request())
            .send()
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);

        // The same forwarded client is limited.
        let third = client
            .post(&url)
            .header("x-forwarded-for", "203.0.113.2, 198.51.100.1")
            .json(&test_request())
            .send()
            .await
            .unwrap();
        assert_eq!(third.status(), StatusCode::TOO_MANY_REQUESTS);

        handle.abort();
    }

    #[tokio::test]
    async fn last_forwarded_header_line_wins_over_spoofed_first() {
        let router = BaseTelemetryServer::router_with_probers(
            Arc::new(AtomicBool::new(true)),
            per_ip(1, vec!["127.0.0.0/8"]),
            DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            reachable_prober(),
            Arc::new(MockClReachabilityProber::new()),
        );
        let (addr, handle) = start_router(router).await;
        let client = reqwest::Client::new();
        let url = format!("http://{addr}{P2P_REACHABILITY_EL_PATH}");

        // The client smuggles its own X-Forwarded-For line; the trusted proxy
        // appends a second line with the real address. The proxy-appended
        // line must key the rate limit.
        let first = client
            .post(&url)
            .header("x-forwarded-for", "203.0.113.1")
            .header("x-forwarded-for", "198.51.100.1")
            .json(&test_request())
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        // Rotating the smuggled first line must not open a fresh bucket.
        let second = client
            .post(&url)
            .header("x-forwarded-for", "203.0.113.2")
            .header("x-forwarded-for", "198.51.100.1")
            .json(&test_request())
            .send()
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);

        handle.abort();
    }

    #[tokio::test]
    async fn missing_forwarded_header_from_trusted_proxy_falls_back_to_peer() {
        let router = BaseTelemetryServer::router_with_probers(
            Arc::new(AtomicBool::new(true)),
            per_ip(1, vec!["127.0.0.0/8"]),
            DEFAULT_P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            reachable_prober(),
            Arc::new(MockClReachabilityProber::new()),
        );
        let (addr, handle) = start_router(router).await;
        let client = reqwest::Client::new();
        let url = format!("http://{addr}{P2P_REACHABILITY_EL_PATH}");

        let first = client.post(&url).json(&test_request()).send().await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        // Without a forwarding header the peer address keys the bucket.
        let second = client.post(&url).json(&test_request()).send().await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);

        handle.abort();
    }
}
