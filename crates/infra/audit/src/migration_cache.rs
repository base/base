//! Serialized pod-cache IO isolated from the async runtime and cached probes.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use tokio::{sync::oneshot, time::timeout};

use crate::{MigrationError, MigrationRecord, MigrationStore};

/// One queued cache write. Dropping its waiter revokes publication before rename.
#[derive(Debug)]
pub struct MigrationCacheWrite {
    /// Immutable private record captured without holding cache locks during IO.
    pub record: MigrationRecord,
    /// Monotonic cache revision; out-of-order submitters cannot publish older state.
    pub revision: u64,
    /// Set when the caller stops waiting; never authorizes later publication.
    pub revoked: Arc<AtomicBool>,
    /// Completion notification without joining the IO thread.
    pub reply: oneshot::Sender<Result<(), MigrationError>>,
}

/// Revokes an outstanding write when its future is cancelled or times out.
#[derive(Debug)]
pub struct MigrationCachePermit {
    /// Shared publication gate.
    pub revoked: Arc<AtomicBool>,
}

impl Drop for MigrationCachePermit {
    fn drop(&mut self) {
        self.revoked.store(true, Ordering::Release);
    }
}

/// A single ordered writer thread, deliberately outside Tokio's blocking pool.
///
/// A wedged filesystem must not make runtime teardown join a wedged IO task.
/// Serialized writes cannot overwrite a newer committed terminal record with an
/// older write. Cancelled waiters additionally revoke writes before rename.
#[derive(Clone)]
pub struct MigrationCacheWriter {
    /// Ordered requests, consumed by exactly one dedicated IO thread.
    pub sender: mpsc::Sender<MigrationCacheWrite>,
}

impl fmt::Debug for MigrationCacheWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MigrationCacheWriter { records and paths redacted }")
    }
}

impl MigrationCacheWriter {
    /// Maximum wait for local-cache IO; database cleanup has an independent budget.
    pub const IO_TIMEOUT: Duration = Duration::from_secs(2);

    /// Starts the purpose-specific ordered file writer.
    pub fn new(store: MigrationStore) -> Result<Self, MigrationError> {
        Self::with_sink(move |record, revoked| {
            store.save_record(&record, || !revoked.load(Ordering::Acquire))
        })
    }

    /// Starts an ordered record sink. The sink must honor revocation before commit.
    /// This permits barrier-controlled filesystem fault tests without runtime IO.
    pub fn with_sink(
        mut save: impl FnMut(MigrationRecord, Arc<AtomicBool>) -> Result<(), MigrationError>
        + Send
        + 'static,
    ) -> Result<Self, MigrationError> {
        let (sender, receiver) = mpsc::channel::<MigrationCacheWrite>();
        // Dropping this handle detaches it; process exit, not runtime shutdown,
        // ends a permanently blocked filesystem thread.
        let _ = thread::Builder::new()
            .name("audit-migration-cache".into())
            .spawn(move || {
                let mut latest = 0;
                while let Ok(write) = receiver.recv() {
                    let result = if write.revision < latest || write.revoked.load(Ordering::Acquire)
                    {
                        Err(MigrationError::StateIo)
                    } else {
                        latest = write.revision;
                        save(write.record, write.revoked)
                    };
                    let _ = write.reply.send(result);
                }
            })
            .map_err(|_| MigrationError::StateIo)?;
        Ok(Self { sender })
    }

    /// Waits a bounded time for this ordered write, without blocking runtime threads.
    pub async fn save(&self, record: MigrationRecord, revision: u64) -> Result<(), MigrationError> {
        let (reply, receive) = oneshot::channel();
        let permit = MigrationCachePermit { revoked: Arc::new(AtomicBool::new(false)) };
        self.sender
            .send(MigrationCacheWrite {
                record,
                revision,
                revoked: Arc::clone(&permit.revoked),
                reply,
            })
            .map_err(|_| MigrationError::StateIo)?;
        timeout(Self::IO_TIMEOUT, receive)
            .await
            .map_err(|_| MigrationError::StateIo)?
            .map_err(|_| MigrationError::StateIo)?
    }

    /// Reads the startup cache off-runtime with the same bounded IO wait.
    pub async fn load(
        store: MigrationStore,
        generation: String,
    ) -> Result<Option<MigrationRecord>, MigrationError> {
        let (send, receive) = oneshot::channel();
        let _ = thread::Builder::new()
            .name("audit-migration-cache-read".into())
            .spawn(move || {
                let _ = send.send(store.load(&generation));
            })
            .map_err(|_| MigrationError::StateIo)?;
        timeout(Self::IO_TIMEOUT, receive)
            .await
            .map_err(|_| MigrationError::StateIo)?
            .map_err(|_| MigrationError::StateIo)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuditMigration, MigrationState, MigrationStatus};
    use std::{fs, path::PathBuf};

    #[tokio::test]
    async fn older_submission_cannot_overwrite_committed_terminal_cleanup() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp").join(format!(
            "{}-audit-cache-order-{}",
            chrono::Utc::now().format("%F"),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&path).unwrap();
        let store = MigrationStore { path: path.join("state.json") };
        let writer = MigrationCacheWriter::new(store.clone()).unwrap();
        let mut record = MigrationRecord {
            fingerprint: AuditMigration::fingerprint(),
            target: None,
            status: MigrationStatus::new("cache-order".into()),
            backend: None,
        };
        record.status.state = MigrationState::Stopped;
        record.status.cleanup_confirmed = true;
        writer.save(record.clone(), 2).await.unwrap();
        record.status.state = MigrationState::Running;
        record.status.cleanup_confirmed = false;
        assert_eq!(writer.save(record, 1).await.unwrap_err().code(), "state_io");
        let restored = store.load("cache-order").unwrap().unwrap();
        assert_eq!(restored.status.state, MigrationState::Stopped);
        assert!(restored.status.cleanup_confirmed);
    }
}
