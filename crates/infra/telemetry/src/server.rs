use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Context;
use axum::Router;
use base_health::HealthServer;
use clap::Args;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{P2P_REACHABILITY_MAX_CONCURRENT_PROBES, P2pRoutes, RlpxProber};

/// Configuration for the Base telemetry HTTP server.
#[derive(Args, Debug, Clone)]
pub struct ServerConfig {
    /// Socket address to bind the HTTP server to.
    #[arg(long, env = "BASE_TELEMETRY_LISTEN_ADDR", default_value = "0.0.0.0:8080")]
    pub listen_addr: SocketAddr,
}

/// Base telemetry Axum server scaffold.
#[derive(Debug, Clone, Copy, Default)]
pub struct BaseTelemetryServer;

impl BaseTelemetryServer {
    /// Returns the application router for the telemetry service.
    pub fn router(ready: Arc<AtomicBool>) -> Router {
        HealthServer::router(ready).merge(P2pRoutes::router_with_prober(
            P2P_REACHABILITY_MAX_CONCURRENT_PROBES,
            Arc::new(RlpxProber::ephemeral()),
        ))
    }

    /// Starts the telemetry service with the provided configuration.
    pub async fn serve(config: ServerConfig, cancel: CancellationToken) -> anyhow::Result<()> {
        let ServerConfig { listen_addr } = config;
        let ready = Arc::new(AtomicBool::new(false));
        let app = Self::router(Arc::clone(&ready));
        let listener = TcpListener::bind(listen_addr)
            .await
            .with_context(|| format!("failed to bind base telemetry server to {listen_addr}"))?;
        let listen_addr =
            listener.local_addr().context("failed to read base telemetry listen address")?;

        ready.store(true, Ordering::SeqCst);

        info!(listen_addr = %listen_addr, "base telemetry server started");

        axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await
            .context("base telemetry server exited unexpectedly")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{Arc, atomic::AtomicBool},
        time::Duration,
    };

    use async_trait::async_trait;
    use axum::{Router, http::StatusCode};
    use base_health::HealthServer;
    use tokio::{net::TcpListener, sync::Semaphore, task::JoinHandle};

    use crate::{
        BaseTelemetryServer, P2P_REACHABILITY_PATH, P2pReachabilityRequest, P2pRoutes,
        ReachabilityProber, RlpxProbeOutcome, RlpxProbeResult, RlpxProbeStage, RlpxProbeTarget,
        TEST_NODE_ID,
    };

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

    async fn start_router(router: Router) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (addr, handle)
    }

    async fn start_test_server() -> (SocketAddr, JoinHandle<()>) {
        start_router(BaseTelemetryServer::router(Arc::new(AtomicBool::new(true)))).await
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
        let router = HealthServer::router(Arc::new(AtomicBool::new(true)))
            .merge(P2pRoutes::router_with_prober(1, Arc::clone(&prober)));
        let (addr, handle) = start_router(router).await;
        let request =
            P2pReachabilityRequest { enode: format!("enode://{TEST_NODE_ID}@8.8.8.8:30303") };
        let client = reqwest::Client::new();
        let probe_client = client.clone();

        let probe = tokio::spawn(async move {
            probe_client
                .post(format!("http://{addr}{P2P_REACHABILITY_PATH}"))
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
}
