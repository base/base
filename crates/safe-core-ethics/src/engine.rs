use async_trait::async_trait;

use crate::{EthicsEngine, EthicsError, EthicsRule, EthicsVerdict};

pub struct DefaultEthicsEngine;

#[async_trait]
impl EthicsEngine for DefaultEthicsEngine {
    async fn evaluate(
        &self,
        _action: &str,
        _context: &serde_json::Value,
    ) -> Result<EthicsVerdict, EthicsError> {
        Ok(EthicsVerdict {
            verdict: "Allow".to_string(),
            reason: "Default allow".to_string(),
            rule_id: None,
        })
    }

    async fn load_rules(&mut self, _rules: Vec<EthicsRule>) -> Result<(), EthicsError> {
        Ok(())
    }

    async fn list_rules(&self) -> Vec<EthicsRule> {
        vec![]
    }
}
