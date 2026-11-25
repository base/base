//! Batched persistence of canonical blocks for a full state, off the import path as in
//! base-reth: validated canonical blocks stay in memory ([`crate::Chain`]'s in-memory layer) and
//! this thread commits them in batches, holding neither the engine lock nor the chain lock while
//! it writes. A crash loses only the blocks in memory: the datadir reopens at the last batch and
//! the consensus node sends the rest again. On SIGTERM or SIGINT it persists every canonical block
//! through the head and exits, as base-reth does before shutdown.

use std::{
    sync::{Arc, Condvar, Mutex, atomic::AtomicBool, atomic::Ordering},
    thread,
    time::Duration,
};

use crate::Node;

/// How often the persistence thread checks for a batch.
const POLL: Duration = Duration::from_millis(20);
/// How long the persistence thread waits after a failed batch commit before it retries.
const RETRY: Duration = Duration::from_secs(1);
/// `SIGINT` and `SIGTERM` (the same numbers on Linux and macOS).
const SHUTDOWN_SIGNALS: [i32; 2] = [2, 15];

/// Set by the signal handler; the persistence thread drains and exits.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    /// The C library's `signal(2)`, taking and returning a handler address.
    fn signal(signum: i32, handler: usize) -> usize;
}

/// Async-signal-safe: only stores a flag.
extern "C" fn on_shutdown_signal(_signum: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// The persistence thread's state.
#[derive(Debug, Default)]
pub struct Persistence {
    running: AtomicBool,
    batches: Mutex<u64>,
    persisted: Condvar,
}

impl Node {
    /// Starts the persistence thread, which also handles SIGTERM and SIGINT. A partial state
    /// commits each block, so it has no batches and only exits on a signal.
    pub fn start_persistence(self: &Arc<Self>) -> Result<(), String> {
        let batched = !self.chain.read().expect("chain lock").db.is_partial();
        assert!(!self.persistence.running.swap(batched, Ordering::SeqCst), "persistence starts once");
        let node = Arc::clone(self);
        thread::Builder::new()
            .name("persistence".into())
            .spawn(move || {
                loop {
                    if SHUTDOWN.load(Ordering::SeqCst) {
                        node.persist_and_exit();
                    }
                    match node.persist_once() {
                        Ok(true) => {}
                        Ok(false) => thread::sleep(POLL),
                        Err(error) => {
                            // The blocks were validated, so this is a datadir fault, such as a
                            // full disk; the failed commit healed the datadir back to the last
                            // batch, so try again (the blocks stay in memory meanwhile).
                            eprintln!("tiger: persisting blocks failed, retrying: {error}");
                            thread::sleep(RETRY);
                        }
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        for signum in SHUTDOWN_SIGNALS {
            // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
            let previous = unsafe { signal(signum, on_shutdown_signal as extern "C" fn(i32) as usize) };
            // `SIG_ERR` is -1.
            if previous == usize::MAX {
                return Err(format!("installing the handler of signal {signum} failed"));
            }
        }
        Ok(())
    }

    /// Stops block imports, persists every canonical block through the head (base-reth's
    /// `persist_until_complete` on shutdown) and exits.
    fn persist_and_exit(&self) -> ! {
        // Holding the engine lock keeps the Engine API and the sync thread from moving the head.
        let _engine = self.engine.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let unbatched = self.chain.write().expect("chain lock").set_persistence(0, 0);
        let drained = unbatched.and_then(|()| {
            while self.persist_once()? {}
            Ok(self.chain.read().expect("chain lock").unpersisted())
        });
        match drained {
            Ok(0) => {
                eprintln!("tiger: persisted every block through the head, shutting down");
                std::process::exit(0);
            }
            Ok(left) => {
                eprintln!("tiger: shutting down with {left} blocks unpersisted");
                std::process::exit(1);
            }
            Err(error) => {
                eprintln!("tiger: persisting blocks before shutdown failed: {error}");
                std::process::exit(1);
            }
        }
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
