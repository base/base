//! A dedicated runtime that serves the archive's async reads to synchronous EVM state reads.
//!
//! reth runs execution closures on a tokio blocking thread inside an outer `block_on`, so a
//! state provider cannot `block_on` (it would panic) or wait on a task of the runtime whose
//! blocking pool it occupies. Each read is spawned onto this runtime instead and waited for
//! with a bounded synchronous receive; node reads the reader makes run on this runtime's own
//! blocking pool.

use std::{
    cell::Cell,
    sync::mpsc::{self, RecvTimeoutError},
    time::Instant,
};

use base_exoware_archive::ArchiveError;
use tokio::runtime::{Builder, Runtime};

thread_local! {
    /// Set on every thread of an [`ArchiveExecutor`] runtime.
    static ON_ARCHIVE_EXEC: Cell<bool> = const { Cell::new(false) };
}

/// The archive's execution runtime.
#[derive(Debug)]
pub struct ArchiveExecutor {
    /// The runtime; taken only on drop.
    runtime: Option<Runtime>,
}

impl ArchiveExecutor {
    /// Worker threads of the runtime. Reads are I/O bound; the workers only drive them.
    pub const WORKERS: usize = 2;

    /// Starts the runtime.
    pub fn new() -> std::io::Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(Self::WORKERS)
            .thread_name("archive-exec")
            .enable_all()
            .on_thread_start(|| ON_ARCHIVE_EXEC.set(true))
            .build()?;
        Ok(Self { runtime: Some(runtime) })
    }

    /// Runs `read` on the runtime and waits for it, at most until `deadline`. A read past the
    /// deadline is never submitted; one that outlives it is aborted. Both are store errors. Called from one of the
    /// runtime's own threads it fails at once instead of waiting on itself.
    pub fn run<T: Send + 'static>(
        &self,
        read: impl Future<Output = Result<T, ArchiveError>> + Send + 'static,
        deadline: Instant,
    ) -> Result<T, ArchiveError> {
        if ON_ARCHIVE_EXEC.get() {
            return Err(ArchiveError::Local {
                reason: "archive state read from an archive-exec thread".to_string(),
            });
        }
        let Some(runtime) = &self.runtime else {
            return Err(ArchiveError::Local { reason: "archive runtime is shut down".to_string() });
        };
        if Instant::now() >= deadline {
            return Err(ArchiveError::store("request", "execution deadline exceeded"));
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let task = runtime.spawn(async move {
            let _ = tx.send(read.await);
        });
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                task.abort();
                Err(ArchiveError::store("request", "execution deadline exceeded"))
            }
            Err(RecvTimeoutError::Disconnected) => Err(ArchiveError::Local {
                reason: "archive read ended without an answer".to_string(),
            }),
        }
    }
}

impl Drop for ArchiveExecutor {
    /// Shuts the runtime down without waiting for its tasks.
    fn drop(&mut self) {
        // Dropping a runtime blocks, which panics inside an async context; never wait here.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The deadline the tests use for reads that finish.
    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    /// A read from a tokio blocking thread inside `block_on` (reth's execution spawner) is
    /// answered, including a read that needs the executor's own blocking pool.
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_from_reths_spawner_path_are_answered() {
        let executor = std::sync::Arc::new(ArchiveExecutor::new().expect("runtime"));
        let handle = tokio::runtime::Handle::current();
        let inner = std::sync::Arc::clone(&executor);
        let answer = tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                inner.run(
                    async {
                        tokio::task::spawn_blocking(|| 7u64)
                            .await
                            .map_err(|err| ArchiveError::Local { reason: err.to_string() })
                    },
                    soon(),
                )
            })
        })
        .await
        .expect("blocking task joins");
        assert_eq!(answer, Ok(7), "the read is answered from inside reth's block_on");
    }

    /// A read past the deadline is aborted and reported as a store error, without waiting
    /// for the read.
    #[test]
    fn deadline_aborts_the_read() {
        let executor = ArchiveExecutor::new().expect("runtime");
        let started = Instant::now();
        let (dropped_tx, dropped_rx) = mpsc::channel::<()>();
        let result: Result<(), _> = executor.run(
            async move {
                let _guard = dropped_tx;
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(())
            },
            Instant::now() + Duration::from_millis(50),
        );
        assert_eq!(
            result,
            Err(ArchiveError::store("request", "execution deadline exceeded")),
            "a late read is a store error"
        );
        assert!(started.elapsed() < Duration::from_secs(5), "the caller does not wait for it");
        assert_eq!(
            dropped_rx.recv_timeout(Duration::from_secs(5)),
            Err(RecvTimeoutError::Disconnected),
            "the aborted read is dropped"
        );
    }

    /// A read started on the executor's own threads fails instead of deadlocking, and a
    /// panicking read is an error, not a hang.
    #[test]
    fn self_calls_and_panics_fail_fast() {
        let executor = std::sync::Arc::new(ArchiveExecutor::new().expect("runtime"));
        let inner = std::sync::Arc::clone(&executor);
        let nested = executor.run(
            async move { Ok(inner.run(async { Ok(1u8) }, Instant::now() + Duration::from_secs(5))) },
            soon(),
        );
        assert_eq!(
            nested,
            Ok(Err(ArchiveError::Local {
                reason: "archive state read from an archive-exec thread".to_string()
            })),
            "a nested read on an archive-exec thread is refused"
        );
        let panicked: Result<(), _> = executor.run(async { panic!("read panicked") }, soon());
        assert_eq!(
            panicked,
            Err(ArchiveError::Local { reason: "archive read ended without an answer".to_string() }),
            "a panicking read is an error"
        );
    }
}
