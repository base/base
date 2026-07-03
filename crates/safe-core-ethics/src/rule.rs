use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EthicsRule {
    pub id: String,
    pub action: String,
    pub constraint: String,
    pub severity: Severity,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Severity {
    Block,
    RequireApproval,
    Allow,
}
