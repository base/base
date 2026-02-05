//! Warp HTTP server for serving static files and SSE events.

use std::{convert::Infallible, net::SocketAddr, sync::Arc};

use reth_chain_state::CanonStateSubscriptions;
use reth_provider::BlockReader;
use reth_transaction_pool::TransactionPool;
use rust_embed::Embed;
use tracing::info;
use warp::{Filter, Reply, http::header::HeaderValue, hyper::Body, reply::Response};

use crate::data_feed::DataFeed;

/// Embedded frontend assets.
#[derive(Embed)]
#[folder = "frontend/static"]
struct Frontend;

/// Starts the dashboard HTTP server.
pub(crate) async fn start_dashboard_server<Provider, Pool>(
    bind_addr: SocketAddr,
    provider: Provider,
    pool: Pool,
    peers_fn: Arc<dyn Fn() -> (usize, usize, usize) + Send + Sync>,
    network_name: String,
    client_name: String,
) where
    Provider: BlockReader + CanonStateSubscriptions + Clone + Send + Sync + 'static,
    Pool: TransactionPool + Clone + Send + Sync + 'static,
{
    info!(addr = %bind_addr, "Starting dashboard server");

    let data_feed = Arc::new(DataFeed::new(provider, pool, peers_fn));
    data_feed.start(network_name, client_name);

    // SSE events endpoint (matches Nethermind's path for frontend compatibility)
    let events = warp::path!("data" / "events")
        .and(warp::get())
        .and(warp::any().map(move || Arc::clone(&data_feed)))
        .map(|data_feed: Arc<DataFeed<Provider, Pool>>| {
            let stream = data_feed.subscribe();
            warp::sse::reply(warp::sse::keep_alive().stream(stream))
        });

    // Static files (root serves index.html)
    let static_files = warp::get().and(warp::path::tail()).and_then(serve_static);

    // Combine routes - SSE first, then static files
    let routes = events.or(static_files);

    info!(addr = %bind_addr, "Dashboard server listening");
    warp::serve(routes).run(bind_addr).await;
}

/// Serves static files from embedded assets.
async fn serve_static(path: warp::path::Tail) -> Result<impl Reply, Infallible> {
    let path = path.as_str();
    let file_path = if path.is_empty() { "index.html" } else { path };

    match Frontend::get(file_path) {
        Some(content) => {
            let mime = mime_guess::from_path(file_path).first_or_octet_stream();
            let mut response = Response::new(Body::from(content.data.into_owned()));
            response.headers_mut().insert(
                "content-type",
                HeaderValue::from_str(mime.as_ref())
                    .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
            );
            Ok(response)
        }
        None => {
            // Try index.html for SPA routing
            match Frontend::get("index.html") {
                Some(content) => {
                    let mut response = Response::new(Body::from(content.data.into_owned()));
                    response
                        .headers_mut()
                        .insert("content-type", HeaderValue::from_static("text/html"));
                    Ok(response)
                }
                None => {
                    let mut response = Response::new(Body::from("Not Found"));
                    *response.status_mut() = warp::http::StatusCode::NOT_FOUND;
                    Ok(response)
                }
            }
        }
    }
}
