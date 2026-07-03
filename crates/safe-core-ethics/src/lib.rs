// crates/safe-core-ethics/src/lib.rs
//! Motor de Ética para Agentes Autônomos — Verificação de Ações contra Políticas

pub mod engine;
pub mod rule;
pub mod verdict;

use async_trait::async_trait;
pub use rule::{EthicsRule, Severity};
pub use verdict::EthicsVerdict;

/// Trait unificado para motores de ética.
#[async_trait]
pub trait EthicsEngine: Send + Sync {
    /// Avalia uma ação contra as regras carregadas.
    async fn evaluate(
        &self,
        action: &str,
        context: &serde_json::Value,
    ) -> Result<EthicsVerdict, EthicsError>;

    /// Carrega um conjunto de regras.
    async fn load_rules(&mut self, rules: Vec<EthicsRule>) -> Result<(), EthicsError>;

    /// Lista todas as regras ativas.
    async fn list_rules(&self) -> Vec<EthicsRule>;
}

#[derive(Debug, thiserror::Error)]
pub enum EthicsError {
    #[error("Regra não encontrada: {0}")]
    RuleNotFound(String),
    #[error("Ação desconhecida: {0}")]
    UnknownAction(String),
    #[error("Erro de validação: {0}")]
    Validation(String),
    #[error("Erro Lean4: {0}")]
    Lean4(String),
}
