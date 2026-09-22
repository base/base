//! HTTP server for the payload witness cache.

use std::sync::Arc;

use alloy_primitives::B256;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use tokio::net::TcpListener;
use tower::limit::ConcurrencyLimitLayer;
use tracing::{Instrument, debug, info_span, warn};

use crate::{Metrics, WitnessCache, WitnessKey};

/// How many witness responses may be serialized at once.
///
/// Each response is about 15 megabytes, so this bounds that work to roughly 480 megabytes.
const MAX_CONCURRENT_LOOKUPS: usize = 32;

/// Serves cached payload witnesses.
#[derive(Debug)]
pub struct WitnessServer;

impl WitnessServer {
    /// Serves `cache` on `listener` until the listener fails.
    ///
    /// `GET /healthz` returns 200. `GET /witness/{parent}/{digest}` returns the cached
    /// `ExecutionWitness` JSON, or 404 when that key is absent.
    pub async fn serve(cache: Arc<WitnessCache>, listener: TcpListener) -> std::io::Result<()> {
        let app = Router::new()
            .route("/healthz", get(|| async { StatusCode::OK }))
            .route("/witness/{parent}/{digest}", get(get_witness))
            .layer(DefaultBodyLimit::max(1024))
            .layer(ConcurrencyLimitLayer::new(MAX_CONCURRENT_LOOKUPS))
            .with_state(cache);
        axum::serve(listener, app).await
    }
}

async fn get_witness(
    State(cache): State<Arc<WitnessCache>>,
    Path((parent, digest)): Path<(String, String)>,
) -> Response {
    let Ok(parent_hash) = parent.parse::<B256>() else {
        Metrics::lookups_total(Metrics::LOOKUP_INVALID).increment(1);
        warn!(field = "parent", "payload witness request rejected");
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(attributes_digest) = digest.parse::<B256>() else {
        Metrics::lookups_total(Metrics::LOOKUP_INVALID).increment(1);
        warn!(field = "digest", "payload witness request rejected");
        return StatusCode::BAD_REQUEST.into_response();
    };
    respond(cache, parent_hash, attributes_digest)
        .instrument(info_span!("witness_cache_lookup", parent_hash = %parent_hash))
        .await
}

async fn respond(cache: Arc<WitnessCache>, parent_hash: B256, attributes_digest: B256) -> Response {
    cache.get(WitnessKey { parent_hash, attributes_digest }).map_or_else(
        || {
            Metrics::lookups_total(Metrics::LOOKUP_MISS).increment(1);
            debug!(parent_hash = %parent_hash, "payload witness cache miss");
            StatusCode::NOT_FOUND.into_response()
        },
        |witness| {
            Metrics::lookups_total(Metrics::LOOKUP_HIT).increment(1);
            debug!(parent_hash = %parent_hash, "payload witness cache hit");
            Json(witness).into_response()
        },
    )
}
