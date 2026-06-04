use async_trait::async_trait;

use crate::OutboxTask;

/// Abstraction for submitting tasks to a processing queue.
///
/// Implementations should be idempotent for a given outbox task. If the
/// processor successfully submits a task but cannot mark the outbox entry as
/// processed, the same task may be submitted again on a later poll.
#[async_trait]
pub trait TaskQueue: Send + Sync {
    /// Submit a task for processing.
    async fn submit(&self, task: OutboxTask) -> anyhow::Result<()>;
}
