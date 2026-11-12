//! Real txpool source and structured observation of the pinned worker's populated snapshots.

use std::sync::{Arc, Mutex};
use alloy_primitives::B256;
use base_common_consensus::BasePrimitives;
use reth_engine_tree::tree::{TxPoolPrewarmSource, TxPoolPrewarmTransaction, TxPoolPrewarmTransactions};

/// Observes actual snapshot publication rather than assuming a sleep warmed the pool.
#[derive(Debug, Clone, Default)]
pub struct SnapshotEvents(
    /// Published account, storage-slot and bytecode counts in event order.
    pub Arc<Mutex<Vec<(u64, u64, u64)>>>,
);
impl tracing::Subscriber for SnapshotEvents {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool { metadata.target() == "engine::tree::txpool_prewarm" }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id { tracing::span::Id::from_u64(1) }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = SnapshotFields::default(); event.record(&mut fields);
        if fields.message == "published txpool prewarming snapshot" { self.0.lock().unwrap().push((fields.accounts, fields.storage, fields.bytecodes)); }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
/// Structured worker publication fields.
#[derive(Debug, Default)]
pub struct SnapshotFields {
    /// Exact static tracing message.
    pub message: String,
    /// Cached accounts.
    pub accounts: u64,
    /// Cached storage slots.
    pub storage: u64,
    /// Cached bytecodes.
    pub bytecodes: u64,
}
impl tracing::field::Visit for SnapshotFields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) { if field.name() == "message" { self.message = format!("{value:?}"); } }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) { match field.name() { "accounts" => self.accounts = value, "storage" => self.storage = value, "bytecodes" => self.bytecodes = value, _ => {} } }
}
/// A real finite pool snapshot containing one signed transfer, not an empty iterator.
#[derive(Debug, Clone)]
pub struct PopulatedPrewarmSource(
    /// The exact signed and recovered transaction exposed to the worker.
    pub TxPoolPrewarmTransaction<BasePrimitives>,
);
impl TxPoolPrewarmSource<BasePrimitives> for PopulatedPrewarmSource {
    fn best_transactions(&self, _: B256) -> Option<TxPoolPrewarmTransactions<BasePrimitives>> { Some(Box::new(std::iter::once(self.0.clone()))) }
}
