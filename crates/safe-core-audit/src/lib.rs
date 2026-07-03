// crates/safe-core-audit/src/lib.rs
//! Auditoria Imutável — Cadeia de Merkle para Decisões de Governança

pub mod event;
pub mod merkle;
pub mod trail;

pub use event::{AuditEvent, EventType};
pub use merkle::{MerkleProof, MerkleTree};
pub use trail::{AuditError, AuditTrail};
