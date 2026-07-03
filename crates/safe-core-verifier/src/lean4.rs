use async_trait::async_trait;
use safe_core_ethics::{EthicsEngine, EthicsError, EthicsRule, EthicsVerdict};

pub struct Lean4Verifier;

impl Lean4Verifier {
    pub fn new(_config: Option<()>) -> Self {
        Self
    }
}

#[async_trait]
impl EthicsEngine for Lean4Verifier {
    async fn evaluate(
        &self,
        _action: &str,
        _context: &serde_json::Value,
    ) -> Result<EthicsVerdict, EthicsError> {
        Ok(EthicsVerdict { verdict: "Allow".to_string(), reason: "".to_string(), rule_id: None })
    }

    async fn load_rules(&mut self, _rules: Vec<EthicsRule>) -> Result<(), EthicsError> {
        Ok(())
    }

    async fn list_rules(&self) -> Vec<EthicsRule> {
        vec![]
    }
}
