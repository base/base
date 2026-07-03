// crates/safe-core-audit/src/lib.rs
//! Auditoria Imutável — Cadeia de Merkle para Decisões de Governança

pub mod merkle;
pub mod event;
pub mod trail;

pub use merkle::{MerkleTree, MerkleProof};
pub use event::{AuditEvent, EventType};
pub use trail::{AuditTrail, AuditError};
