//! Bookkeeping and completion handling for background registration tasks.

use std::{collections::HashMap, future::Future};

use alloy_primitives::Address;
use tokio::task::{self, JoinError, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{RegistrarMetrics, Result};

/// State for a registration task currently in flight.
///
/// One entry per signer address. The pending map is keyed by [`Address`] so
/// each signer has at most one active registration task.
#[derive(Debug)]
pub struct PendingRegistration {
    /// Originating instance ID, used to preserve tasks when the source
    /// instance is unresolved and to attribute log lines.
    pub instance_id: String,
    /// `JoinSet` task id for this registration task.
    pub task_id: task::Id,
    /// Cooperative cancel handle for this single task.
    pub cancel: CancellationToken,
}

/// Driver-owned set of in-flight registration tasks.
#[derive(Debug, Default)]
pub struct RegistrationTaskSet {
    tasks: JoinSet<(Address, Result<()>)>,
    /// Pending registration tasks keyed by signer address.
    pending: HashMap<Address, PendingRegistration>,
}

impl RegistrationTaskSet {
    /// Returns pending signers and their source instance IDs, including cancelled tasks.
    pub fn pending(&self) -> impl ExactSizeIterator<Item = (&Address, &str)> {
        self.pending.iter().map(|(signer, task)| (signer, task.instance_id.as_str()))
    }

    /// Returns whether a signer has a pending registration task, including a cancelled one.
    pub fn contains(&self, signer: &Address) -> bool {
        self.pending.contains_key(signer)
    }

    /// Spawns a registration task and tracks it until its join outcome is consumed.
    ///
    /// Does nothing if the signer already has a pending task, including a cancelled one.
    pub fn spawn<F>(
        &mut self,
        signer: Address,
        instance_id: String,
        cancel: CancellationToken,
        future: F,
    ) where
        F: Future<Output = Result<()>> + Send + 'static,
    {
        if self.pending.contains_key(&signer) {
            return;
        }

        let handle = self.tasks.spawn(async move { (signer, future.await) });
        self.pending
            .insert(signer, PendingRegistration { instance_id, task_id: handle.id(), cancel });
        RegistrarMetrics::proof_tasks_spawned().increment(1);
    }

    /// Cancels a signer's registration task when it is no longer registerable.
    pub fn cancel(&self, signer: &Address) {
        let Some(task) = self.pending.get(signer) else {
            return;
        };
        if task.cancel.is_cancelled() {
            return;
        }
        info!(
            signer = %signer,
            instance = %task.instance_id,
            "cancelling registration task: signer no longer registerable"
        );
        task.cancel.cancel();
        RegistrarMetrics::proof_tasks_cancelled().increment(1);
    }

    /// Drains every task that has already finished from `tasks`.
    pub fn reap_finished_tasks(&mut self) {
        while let Some(joined) = self.tasks.try_join_next_with_id() {
            self.apply_join_outcome(joined);
        }
    }

    /// Consumes one `JoinSet` outcome and updates `pending` plus metrics.
    fn apply_join_outcome(
        &mut self,
        joined: std::result::Result<(task::Id, (Address, Result<()>)), JoinError>,
    ) {
        RegistrarMetrics::proof_tasks_completed().increment(1);
        match joined {
            Ok((id, (signer, result))) => {
                let removed = self.pending.remove(&signer);
                let was_cancelled =
                    removed.as_ref().is_some_and(|entry| entry.cancel.is_cancelled());
                let instance_id =
                    removed.as_ref().map_or("missing", |entry| entry.instance_id.as_str());
                match result {
                    Ok(()) => {
                        RegistrarMetrics::record_proof_task_completed(if was_cancelled {
                            RegistrarMetrics::PROOF_TASK_OUTCOME_CANCELLED
                        } else {
                            RegistrarMetrics::PROOF_TASK_OUTCOME_SUCCEEDED
                        });
                        debug!(
                            task_id = ?id,
                            signer = %signer,
                            instance = %instance_id,
                            pending_entry_found = removed.is_some(),
                            "registration task completed"
                        );
                    }
                    Err(e) => {
                        RegistrarMetrics::record_proof_task_completed(
                            RegistrarMetrics::PROOF_TASK_OUTCOME_FAILED,
                        );
                        warn!(
                            task_id = ?id,
                            error = %e,
                            signer = %signer,
                            instance = %instance_id,
                            pending_entry_found = removed.is_some(),
                            "registration task failed"
                        );
                        RegistrarMetrics::processing_errors_total().increment(1);
                    }
                }
            }
            Err(join_err) => {
                RegistrarMetrics::record_proof_task_completed(
                    RegistrarMetrics::PROOF_TASK_OUTCOME_JOIN_ERROR,
                );
                let id = join_err.id();
                let removed = self.pending.extract_if(|_, pending| pending.task_id == id).next();
                warn!(
                    task_id = ?id,
                    error = %join_err,
                    signer = ?removed.as_ref().map(|(signer, _)| *signer),
                    instance = ?removed.as_ref().map(|(_, task)| task.instance_id.as_str()),
                    pending_entry_found = removed.is_some(),
                    "registration task join error"
                );
                RegistrarMetrics::processing_errors_total().increment(1);
            }
        }
    }

    /// Cancels every pending task cooperatively and awaits natural completion.
    pub async fn drain(&mut self) {
        for task in self.pending.values_mut() {
            if !task.cancel.is_cancelled() {
                RegistrarMetrics::proof_tasks_cancelled().increment(1);
                task.cancel.cancel();
            }
        }
        while let Some(joined) = self.tasks.join_next_with_id().await {
            self.apply_join_outcome(joined);
        }
        RegistrarMetrics::proof_tasks_pending().set(0.0);
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use tokio_util::sync::CancellationToken;

    use super::RegistrationTaskSet;

    #[tokio::test]
    async fn duplicate_spawn_keeps_original_task_pending() {
        let mut tasks = RegistrationTaskSet::default();
        let signer = Address::repeat_byte(1);
        let cancel = CancellationToken::new();
        tasks.spawn(signer, "a".into(), cancel.clone(), async move {
            cancel.cancelled().await;
            Ok(())
        });
        tasks.spawn(signer, "b".into(), CancellationToken::new(), async { Ok(()) });

        assert_eq!(tasks.pending().collect::<Vec<_>>(), vec![(&signer, "a")]);

        tasks.drain().await;
        assert_eq!(tasks.pending().len(), 0);
    }

    #[tokio::test]
    async fn cancelled_task_stays_pending_until_drained() {
        let mut tasks = RegistrationTaskSet::default();
        let signer = Address::repeat_byte(1);
        let cancel = CancellationToken::new();
        tasks.spawn(signer, "instance".into(), cancel.clone(), async move {
            cancel.cancelled().await;
            Ok(())
        });

        tasks.cancel(&signer);
        assert_eq!(tasks.pending().collect::<Vec<_>>(), vec![(&signer, "instance")]);

        tasks.drain().await;
        assert_eq!(tasks.pending().len(), 0);
    }
}
