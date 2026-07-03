//! safe-core-governance — A Camada de Governança para Agentes Autônomos
//!
//! # Os 4 Pilares
//!
//! 1. **Ética**: Verificação de ações contra políticas (enforce_action)
//! 2. **Persistência**: Estado imortal via SQLite (rules, workflows, metrics)
//! 3. **Verificação**: Prova formal via Lean4 (constraints)
//! 4. **Auditoria**: Trilha imutável com Merkle (audit trail)

pub mod governance;
pub mod mcp;

pub use governance::GovernanceEngine;
pub use mcp::GovernanceMcpServer;
// Re-export dos pilares
pub use safe_core_audit as audit;
pub use safe_core_ethics as ethics;
pub use safe_core_persistence as persistence;
pub use safe_core_verifier as verifier;
