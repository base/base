use std::sync::Arc;

use safe_core_ethics::EthicsError;
use serde_json::json;

use crate::{api::*, state::BridgeState};

pub async fn enforce_action(
    state: &Arc<BridgeState>,
    action: &str,
    context: &serde_json::Value,
) -> Result<EnforceResponse, EthicsError> {
    let result = state.ethics_engine.check_action(action, context).await?;
    Ok(EnforceResponse {
        allowed: result.verdict != "Block",
        result: None,
        request_id: uuid::Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        latency_ms: 0,
    })
}

pub async fn get_violations(_state: &Arc<BridgeState>) -> ViolationsResponse {
    ViolationsResponse { total: 0, violations: vec![], timestamp: chrono::Utc::now().to_rfc3339() }
}

pub async fn clear_violations(_state: &Arc<BridgeState>) -> serde_json::Value {
    json!({"ok": true, "cleared": true})
}

pub fn list_invariants(_state: &Arc<BridgeState>) -> InvariantsResponse {
    InvariantsResponse { total: 0, invariants: vec![], timestamp: chrono::Utc::now().to_rfc3339() }
}

pub async fn export_invariants(_state: &Arc<BridgeState>) -> Result<serde_json::Value, String> {
    Ok(json!({"ok": true, "path": "/tmp/safe-core-lean4-export", "note": "Pseudo-código Lean 4"}))
}

pub async fn health_check(state: &Arc<BridgeState>) -> HealthResponse {
    let constraints = state.ethics_engine.constraint_count().await;
    HealthResponse {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        components: HealthComponents {
            ethics_engine: "ready".into(),
            invariants: "ready".into(),
            total_constraints: constraints,
            total_invariants: state.invariants.len(),
        },
        timestamp: chrono::Utc::now().to_rfc3339(),
    }
}
