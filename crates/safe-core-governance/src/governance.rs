use std::sync::Arc;

use safe_core_ethics::{EthicsEngine, EthicsVerdict};
use safe_core_verifier::{Constraint, ConstraintResult, Lean4Verifier, Verifier};
use tokio::sync::RwLock;
use tracing::info;

use crate::{
    audit::{AuditEvent, AuditTrail, EventType},
    persistence::StateRepository,
};

/// A Engine Unificada de Governança.
///
/// Contém os 4 pilares em uma única estrutura, pronta para ser
/// exposta via MCP ou usada diretamente.
pub struct GovernanceEngine {
    /// Pilar 1: Motor de Ética
    pub ethics: Arc<RwLock<dyn EthicsEngine>>,

    /// Pilar 2: Persistência de Estado
    pub repository: Arc<StateRepository>,

    /// Pilar 3: Verificador Formal
    pub verifier: Arc<dyn Verifier>,

    /// Pilar 4: Trilha de Auditoria
    pub audit: Arc<RwLock<AuditTrail>>,
}

impl GovernanceEngine {
    /// Cria uma nova engine com configuração padrão.
    pub async fn new(database_url: &str) -> Result<Self, GovernanceError> {
        // 1. Persistência
        let repository = Arc::new(StateRepository::new(database_url).await?);

        // 2. Ética (carrega regras do banco)
        let rules = repository.load_all_rules().await?;
        let mut ethics = Lean4Verifier::new(None);
        ethics.load_rules(rules).await?;
        let ethics = Arc::new(RwLock::new(ethics));

        // 3. Verificador (placeholder — em produção, usar Lean4 real)
        let verifier = Arc::new(SimpleVerifier);

        // 4. Auditoria
        let audit = Arc::new(RwLock::new(AuditTrail::new()));

        info!("GovernanceEngine inicializada com {} regras", repository.count_rules().await?);

        Ok(Self { ethics, repository, verifier, audit })
    }

    /// Avalia uma ação (Pilar 1 + Pilar 4).
    pub async fn enforce_action(
        &self,
        action: &str,
        context: &serde_json::Value,
        agent_id: Option<&str>,
    ) -> Result<EthicsVerdict, GovernanceError> {
        let engine = self.ethics.read().await;
        let verdict = engine.evaluate(action, context).await?;

        // Registra no audit trail (Pilar 4)
        let event = AuditEvent {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now(),
            event_type: EventType::ActionEvaluated,
            action: action.to_string(),
            verdict: format!("{:?}", verdict),
            rule_id: verdict.rule_id.clone(),
            agent_id: agent_id.map(|s| s.to_string()),
            signature: None,
        };
        let mut audit = self.audit.write().await;
        audit.push(event)?;

        Ok(verdict)
    }

    /// Verifica uma restrição (Pilar 3).
    pub fn verify_constraint(
        &self,
        constraint: &Constraint,
        context: &serde_json::Value,
    ) -> ConstraintResult {
        self.verifier.verify(constraint, context)
    }

    /// Obtém a raiz da Merkle (Pilar 4).
    pub fn audit_root(&self) -> Option<[u8; 32]> {
        self.audit.blocking_read().root()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GovernanceError {
    #[error("Ética: {0}")]
    Ethics(#[from] safe_core_ethics::EthicsError),
    #[error("Persistência: {0}")]
    Persistence(#[from] crate::persistence::RepositoryError),
    #[error("Auditoria: {0}")]
    Audit(#[from] crate::audit::AuditError),
    #[error("Verificação: {0}")]
    Verification(String),
}

// Verificador simples para demonstração
struct SimpleVerifier;

impl Verifier for SimpleVerifier {
    fn verify(&self, constraint: &Constraint, context: &serde_json::Value) -> ConstraintResult {
        // Em produção: chamar Lean4 real
        // Aqui, simula validação baseada em regex simples
        let valid = match constraint.expression.as_str() {
            "percentage <= 20" => context
                .get("percentage")
                .and_then(|v| v.as_f64())
                .map(|p| p <= 20.0)
                .unwrap_or(false),
            "amount <= 100000" => context
                .get("amount")
                .and_then(|v| v.as_f64())
                .map(|a| a <= 100000.0)
                .unwrap_or(false),
            _ => true,
        };

        ConstraintResult {
            valid,
            counterexample: if valid { None } else { Some("Violação detectada".to_string()) },
            proof: None,
        }
    }
}
