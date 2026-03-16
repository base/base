use async_trait::async_trait;

use crate::OutboxTask;

/// Abstraction for submitting tasks to a processing queue.
#[async_trait]
pub trait TaskQueue: Send + Sync + Clone {
    /// Submit a task for processing.
    async fn submit(&self, task: OutboxTask) -> anyhow::Result<()>;
}
