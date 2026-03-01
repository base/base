use std::net::SocketAddr;

use axum::{Router, http::StatusCode, response::IntoResponse, routing::get};
use tracing::info;

/// Health check HTTP server.
pub struct HealthServer;

impl HealthServer {
    /// Binds and starts the health check server on the specified address.
    /// Returns the bound address and a handle that can be awaited to run the server.
    pub async fn serve(
        addr: SocketAddr,
    ) -> anyhow::Result<(SocketAddr, tokio::task::JoinHandle<anyhow::Result<()>>)> {
        let app = Router::new().route("/health", get(health));

        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound_addr = listener.local_addr()?;

        info!(
            message = "Health check server bound successfully",
            address = %bound_addr
        );

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await?;
            Ok(())
        });

        Ok((bound_addr, handle))
    }
}

/// Health check handler that always returns 200 OK.
async fn health() -> impl IntoResponse {
    StatusCode::OK
}
