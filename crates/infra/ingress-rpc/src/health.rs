use std::net::SocketAddr;

use axum::{Router, http::StatusCode, response::IntoResponse, routing::get};
use tracing::info;

/// Health check handler that always returns 200 OK
async fn health() -> impl IntoResponse {
    StatusCode::OK
}

/// Bind and start the health check server on the specified address.
/// Returns a handle that can be awaited to run the server.
pub async fn bind_health_server(
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
