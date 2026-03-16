use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

/// A task read from the outbox.
#[derive(Debug, Clone)]
pub struct OutboxTask {
    /// Sequence ID for ordering.
    pub sequence_id: i64,
    /// Associated proof request ID.
    pub proof_request_id: Uuid,
    /// Task parameters as JSON.
    pub params: Value,
}

/// Abstraction for reading from outbox storage.
#[async_trait]
pub trait OutboxReader: Send + Sync + Clone {
    /// Poll for unprocessed tasks up to `batch_size`.
    async fn poll_tasks(&self, batch_size: i64) -> anyhow::Result<Vec<OutboxTask>>;

    /// Mark a task as successfully processed.
    async fn mark_processed(&self, sequence_id: i64) -> anyhow::Result<()>;

    /// Mark a task as failed with an error message.
    async fn mark_error(&self, sequence_id: i64, error_message: String) -> anyhow::Result<()>;
}
