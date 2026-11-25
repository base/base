//! Batched persistence of canonical blocks for a full state, off the import path as in
//! base-reth: validated canonical blocks stay in memory ([`crate::Chain`]'s in-memory layer) and
//! this thread commits them in batches, holding neither the engine lock nor the chain lock while
//! it writes. A crash loses only the blocks in memory: the datadir reopens at the last batch and
//! the consensus node sends the rest again.

use std::{
    sync::{Arc, Condvar, Mutex, atomic::AtomicBool, atomic::Ordering},
    thread,
    time::Duration,
};

use crate::Node;

/// How often the persistence thread checks for a batch.
const POLL: Duration = Duration::from_millis(20);

/// The persistence thread's state.
#[derive(Debug, Default)]
pub struct Persistence {
    running: AtomicBool,
    batches: Mutex<u64>,
    persisted: Condvar,
}

impl Node {
    /// Starts the persistence thread (full states only; a partial state commits each block).
    pub fn start_persistence(self: &Arc<Self>) -> Result<(), String> {
        if self.chain.read().expect("chain lock").db.is_partial() {
            return Ok(());
        }
        assert!(!self.persistence.running.swap(true, Ordering::SeqCst), "persistence starts once");
        let node = Arc::clone(self);
        thread::Builder::new()
            .name("persistence".into())
            .spawn(move || {
                loop {
                    match node.persist_once() {
                        Ok(true) => {}
                        Ok(false) => thread::sleep(POLL),
                        Err(error) => {
                            // The blocks were validated, so this is a datadir fault: stop, and
                            // reopen at the last batch.
                            eprintln!("tiger: persisting blocks failed: {error}");
                            std::process::exit(1);
                        }
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Commits one persistence batch when one is due. Returns whether it committed one.
    pub fn persist_once(&self) -> Result<bool, String> {
        let Some(batch) = self.chain.read().expect("chain lock").persistence_batch() else { return Ok(false) };
        let started = std::time::Instant::now();
        if !batch.commit()? {
            return Ok(false);
        }
        let committed = started.elapsed();
        self.chain.write().expect("chain lock").persisted(&batch)?;
        if std::env::var_os("TIGER_PERSIST_LOG").is_some() {
            let (number, _) = batch.last();
            eprintln!("tiger: persisted blocks {}..={number} in {committed:?}", batch.disk.0 + 1);
        }
        *self.persistence.batches.lock().expect("persistence lock") += 1;
        self.persistence.persisted.notify_all();
        Ok(true)
    }

    /// Waits, holding no lock, while [`crate::Chain::persistence_backlogged`], as base-reth's
    /// engine stalls on persistence.
    pub fn persistence_backpressure(&self) {
        if !self.persistence.running.load(Ordering::SeqCst) {
            return;
        }
        let behind = || self.chain.read().expect("chain lock").persistence_backlogged();
        let mut batches = self.persistence.batches.lock().expect("persistence lock");
        while behind() {
            batches = self.persistence.persisted.wait_timeout(batches, POLL * 5).expect("persistence lock").0;
        }
    }
}
