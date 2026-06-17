//! Diagnostic database wrapper that records the storage/account reads an EVM performs.
//!
//! Used only by the tx-cache shadow-diff (Layer 2): wrapping the pending-execution database lets
//! the shadow re-execution capture the full read-set of a diverging transaction — including
//! storage slots a transaction reads but never writes, which the cached write-set cannot reveal.
//! Recording is off by default and toggled around a single re-execution, so normal execution pays
//! nothing.

use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
};

use alloy_primitives::{Address, B256, U256};
use revm::{
    Database, DatabaseCommit,
    state::{AccountInfo, Bytecode, EvmState},
};

/// Captured reads from a single (toggled) execution window.
///
/// `enabled` gates recording so it is a no-op outside the shadow re-execution. The caller clears
/// the vectors and flips `enabled` on immediately before the re-execution, then snapshots and
/// flips it off immediately after.
#[derive(Debug, Default)]
pub struct ReadLog {
    /// Whether reads are currently being recorded.
    pub enabled: bool,
    /// Recorded storage reads as `(address, slot, value)`.
    pub storage: Vec<(Address, U256, U256)>,
    /// Recorded account reads as `(address, balance, nonce)`.
    pub accounts: Vec<(Address, U256, u64)>,
}

/// A snapshot of reads captured during a single re-execution.
#[derive(Debug, Default, Clone)]
pub struct CapturedReads {
    /// Storage reads as `(address, slot, value)`.
    pub storage: Vec<(Address, U256, U256)>,
    /// Account reads as `(address, balance, nonce)`.
    pub accounts: Vec<(Address, U256, u64)>,
}

/// A [`Database`] decorator that records storage and account reads into a shared [`ReadLog`].
///
/// Forwards every operation to the inner database and, when the shared log is `enabled`, appends
/// the value returned for each `storage`/`basic` lookup. [`Deref`]s to the inner database so
/// inherent methods (e.g. `merge_transitions`, `bundle_state`) remain available transparently.
#[derive(Debug)]
pub struct RecordingDb<D> {
    inner: D,
    log: Arc<Mutex<ReadLog>>,
}

impl<D> RecordingDb<D> {
    /// Wraps `inner`, recording reads into the shared `log` while it is enabled.
    pub const fn new(inner: D, log: Arc<Mutex<ReadLog>>) -> Self {
        Self { inner, log }
    }
}

impl<D> Deref for RecordingDb<D> {
    type Target = D;

    fn deref(&self) -> &D {
        &self.inner
    }
}

impl<D> DerefMut for RecordingDb<D> {
    fn deref_mut(&mut self) -> &mut D {
        &mut self.inner
    }
}

impl<D: Database> Database for RecordingDb<D> {
    type Error = D::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let info = self.inner.basic(address)?;
        if let Ok(mut log) = self.log.lock()
            && log.enabled
            && let Some(info) = &info
        {
            log.accounts.push((address, info.balance, info.nonce));
        }
        Ok(info)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let value = self.inner.storage(address, index)?;
        if let Ok(mut log) = self.log.lock()
            && log.enabled
        {
            log.storage.push((address, index, value));
        }
        Ok(value)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number)
    }
}

impl<D: DatabaseCommit> DatabaseCommit for RecordingDb<D> {
    fn commit(&mut self, changes: EvmState) {
        self.inner.commit(changes);
    }
}
