use safe_core_ethics::{EthicsError, EthicsVerdict};
use serde_json::Value;

pub struct DummyEthicsEngine;

impl DummyEthicsEngine {
    pub async fn check_action(
        &self,
        _action: &str,
        _context: &Value,
    ) -> Result<EthicsVerdict, EthicsError> {
        Ok(EthicsVerdict { verdict: "Allowed".to_string(), reason: "".to_string(), rule_id: None })
    }
    pub async fn get_violations(&self) -> Vec<Value> {
        vec![]
    }
    pub async fn clear_violations(&self) {}
    pub async fn constraint_count(&self) -> usize {
        0
    }
}

pub struct BridgeState {
    pub ethics_engine: DummyEthicsEngine,
    pub invariants: Vec<crate::api::InvariantView>,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self::new()
    }
}

impl BridgeState {
    pub fn new() -> Self {
        Self { ethics_engine: DummyEthicsEngine, invariants: vec![] }
    }
}
