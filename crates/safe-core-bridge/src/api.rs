use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize)]
pub struct EnforceResponse {
    pub allowed: bool,
    pub result: Option<Value>,
    pub request_id: String,
    pub timestamp: String,
    pub latency_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViolationView {
    pub constraint_id: String,
    pub description: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViolationsResponse {
    pub total: usize,
    pub violations: Vec<ViolationView>,
    pub timestamp: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InvariantView {
    pub id: String,
    pub severity: String,
    pub expression: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InvariantsResponse {
    pub total: usize,
    pub invariants: Vec<InvariantView>,
    pub timestamp: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthComponents {
    pub ethics_engine: String,
    pub invariants: String,
    pub total_constraints: usize,
    pub total_invariants: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub components: HealthComponents,
    pub timestamp: String,
}
