use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub event_type: EventType,
    pub action: String,
    pub verdict: String,
    pub rule_id: Option<String>,
    pub agent_id: Option<String>,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventType {
    ActionEvaluated,
    RuleCreated,
    RuleUpdated,
    RuleDeleted,
    WorkflowExecuted,
    SystemStarted,
}
