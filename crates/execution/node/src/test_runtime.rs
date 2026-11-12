use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use reth_provider::providers::RocksDBProvider;
use reth_tasks::Runtime;

/// Owns a test node's runtime and waits for its `RocksDB` database to close.
///
/// Declare this before local node handles, or after node handles in a struct, so those
/// handles are dropped first. Callers must also release any cloned providers before teardown.
/// Teardown warns after a bounded wait if shutdown stalls, including during panic unwinding.
/// Directories are preserved when database closure cannot be confirmed.
/// Poll node launch futures on [`Runtime::handle`] too: upstream components also spawn
/// tasks on the ambient Tokio runtime, not only through the supplied reth executor.
#[derive(Debug)]
pub struct TestNodeRuntime {
    /// Runtime used to launch the node.
    pub runtime: Runtime,
    /// `RocksDB` database path to wait for before completing teardown.
    pub rocksdb_path: Option<PathBuf>,
    /// Temporary directories removed only after database closure is confirmed.
    pub cleanup_paths: Vec<PathBuf>,
}

impl Default for TestNodeRuntime {
    fn default() -> Self {
        // Outside an async context, reth builds an owned runtime with small test thread pools.
        let runtime = thread::spawn(Runtime::test).join().expect("create test node runtime");
        Self { runtime, rocksdb_path: None, cleanup_paths: Vec::new() }
    }
}

impl Drop for TestNodeRuntime {
    fn drop(&mut self) {
        let runtime = self.runtime.clone();
        let rocksdb_path = self.rocksdb_path.take();
        let cleanup_paths = std::mem::take(&mut self.cleanup_paths);
        let (finished, completion) = std::sync::mpsc::channel();
        thread::spawn(move || {
            const TIMEOUT: Duration = Duration::from_secs(5);
            if !runtime.graceful_shutdown_with_timeout(TIMEOUT) {
                tracing::warn!("test node graceful shutdown timed out");
            }
            runtime.shutdown_timeout(TIMEOUT);
            if let Some(path) = rocksdb_path {
                let deadline = Instant::now() + TIMEOUT;
                loop {
                    // RocksDB's own lock registry also detects writers in this process.
                    match RocksDBProvider::builder(&path).with_block_cache_size(1024 * 1024).build()
                    {
                        Ok(database) => {
                            drop(database);
                            break;
                        }
                        Err(error) if Instant::now() >= deadline => {
                            tracing::warn!(path = %path.display(), %error, "test node database did not close");
                            return;
                        }
                        Err(_) => thread::sleep(Duration::from_millis(10)),
                    }
                }
            }
            for path in cleanup_paths {
                if let Err(error) = std::fs::remove_dir_all(&path) {
                    tracing::warn!(path = %path.display(), %error, "test node directory cleanup failed");
                }
            }
            let _ = finished.send(());
        });
        if let Err(error) = completion.recv_timeout(Duration::from_secs(20)) {
            tracing::warn!(%error, "test node teardown did not complete");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn teardown_closes_database_with_live_runtime_clone() {
        let path = reth_db::test_utils::tempdir_path();
        let mut owner = TestNodeRuntime::default();
        owner.rocksdb_path = Some(path.clone());
        let retained_runtime = owner.runtime.clone();
        let database = RocksDBProvider::new(&path).unwrap();
        assert!(RocksDBProvider::new(&path).is_err(), "open database must exclude a second writer");
        let (started, ready) = tokio::sync::oneshot::channel();
        owner.runtime.handle().spawn(async move {
            let _database = database;
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        drop(owner);
        let reopened = RocksDBProvider::new(&path).expect("teardown must close RocksDB");
        drop(reopened);
        drop(retained_runtime);
        std::fs::remove_dir_all(path).unwrap();
    }
}
