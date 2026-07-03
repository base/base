use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EthicsVerdict {
    pub verdict: String,
    pub reason: String,
    pub rule_id: Option<String>,
}
