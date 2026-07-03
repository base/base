use axum::Router;
use clap::Parser;
use safe_core_bridge::{handlers, BridgeState};
use safe_core_bridge::mcp::mcp_impl::SafeCoreMcpServer;
use std::sync::Arc;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

#[derive(Parser)]
#[command(name = "safe-core-bridge", version)]
struct Cli {
    #[arg(long, env = "ADDR", default_value = "0.0.0.0:8081")]
    addr: String,

    #[arg(long, env = "MCP")]
    mcp: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let state = Arc::new(BridgeState::new());

    if cli.mcp {
        let server = SafeCoreMcpServer::new(state);
        server.run_stdio().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    } else {
        let app = Router::new()
            .merge(handlers::router(state.clone()))
            .layer(CorsLayer::permissive())
            .layer(TraceLayer::new_for_http());

        let listener = tokio::net::TcpListener::bind(&cli.addr).await?;
        axum::serve(listener, app).await?;
    }

    Ok(())
}
