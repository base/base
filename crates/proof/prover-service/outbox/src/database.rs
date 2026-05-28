use anyhow::Context;
use async_trait::async_trait;
use base_prover_service_db::{MarkOutboxError, MarkOutboxProcessed, ProofRequestRepo};

use crate::{OutboxReader, OutboxTask};

/// Outbox reader backed by `PostgreSQL` via [`ProofRequestRepo`].
#[derive(Clone)]
pub struct DatabaseOutboxReader {
    repo: ProofRequestRepo,
    max_retries: i32,
}

impl std::fmt::Debug for DatabaseOutboxReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseOutboxReader").field("max_retries", &self.max_retries).finish()
    }
}

impl DatabaseOutboxReader {
    /// Create a new database-backed outbox reader.
    pub const fn new(repo: ProofRequestRepo, max_retries: i32) -> Self {
        Self { repo, max_retries }
    }
}

#[async_trait]
impl OutboxReader for DatabaseOutboxReader {
    async fn poll_tasks(&self, batch_size: u64) -> anyhow::Result<Vec<OutboxTask>> {
        let batch_size =
            i64::try_from(batch_size).context("outbox batch size exceeds i64 range")?;
        let entries =
            self.repo.get_unprocessed_outbox_entries(batch_size, self.max_retries).await?;

        let tasks = entries
            .into_iter()
            .map(|entry| {
                let sequence_id =
                    u64::try_from(entry.sequence_id).context("outbox sequence ID is negative")?;
                Ok(OutboxTask {
                    sequence_id,
                    proof_request_id: entry.proof_request_id,
                    params: entry.request_params,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(tasks)
    }

    async fn mark_processed(&self, sequence_id: u64) -> anyhow::Result<()> {
        let sequence_id =
            i64::try_from(sequence_id).context("outbox sequence ID exceeds i64 range")?;
        self.repo.mark_outbox_processed(MarkOutboxProcessed { sequence_id }).await?;
        Ok(())
    }

    async fn mark_error(&self, sequence_id: u64, error_message: String) -> anyhow::Result<()> {
        let sequence_id =
            i64::try_from(sequence_id).context("outbox sequence ID exceeds i64 range")?;
        self.repo.mark_outbox_error(MarkOutboxError { sequence_id, error_message }).await?;
        Ok(())
    }
}
