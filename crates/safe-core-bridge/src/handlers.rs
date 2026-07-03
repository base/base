use std::sync::Arc;

use axum::{Router, routing::get};

use crate::state::BridgeState;

pub fn router(_state: Arc<BridgeState>) -> Router {
    Router::new().route("/health", get(|| async { "ok" }))
}
