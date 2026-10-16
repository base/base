//! Test utilities for exercising validity metadata storage failures.
//!
//! [`FailingBackend`] is hand-rolled rather than mocked because it implements redb's foreign
//! `StorageBackend` trait and must delegate to a working in-memory backend until a failure is
//! injected.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use redb::{Database, StorageBackend, backends::InMemoryBackend};

use crate::ValidityMetadataStore;

/// In-memory redb backend whose mutations fail while [`FailingBackend::fail`] is set.
#[derive(Debug, Default)]
pub struct FailingBackend {
    inner: InMemoryBackend,
    /// When set, every write, resize and sync returns an I/O error.
    pub fail: Arc<AtomicBool>,
}

impl FailingBackend {
    /// Returns an in-memory store and the switch that makes its writes fail.
    pub fn store() -> (ValidityMetadataStore, Arc<AtomicBool>) {
        let backend = Self::default();
        let fail = Arc::clone(&backend.fail);
        let db = Database::builder().create_with_backend(backend).expect("in-memory database");
        (ValidityMetadataStore::from_database(db).expect("create tables"), fail)
    }

    fn check(&self) -> io::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected storage failure"));
        }
        Ok(())
    }
}

impl StorageBackend for FailingBackend {
    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }

    fn read(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.inner.read(offset, len)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.check()?;
        self.inner.set_len(len)
    }

    fn sync_data(&self, eventual: bool) -> io::Result<()> {
        self.check()?;
        self.inner.sync_data(eventual)
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.check()?;
        self.inner.write(offset, data)
    }
}
