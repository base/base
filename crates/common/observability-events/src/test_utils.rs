//! Records globally emitted transaction events for tests.
//!
//! [`TransactionEventCapture::install`] registers an in-memory writer in the
//! process-global slot when it is empty, clears any events already recorded
//! there, and holds a lock so concurrent capture tests do not interleave.

use std::sync::{Mutex, MutexGuard};

use crate::{GlobalTransactionEventWriter, TransactionEvent, TransactionEventWriter};

static TEST_CAPTURE_SERIAL: Mutex<()> = Mutex::new(());

/// Process-global capture of emitted transaction events for tests.
#[derive(Debug)]
pub struct TransactionEventCapture {
    _serial: MutexGuard<'static, ()>,
    writer: &'static TransactionEventWriter,
}

impl TransactionEventCapture {
    /// Installs a process-global in-memory writer and starts a fresh recording window.
    pub fn install() -> Self {
        let serial = TEST_CAPTURE_SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        let writer =
            GlobalTransactionEventWriter::get_or_init(TransactionEventWriter::in_memory("test"));
        writer.clear_recorded_events();
        Self { _serial: serial, writer }
    }

    /// Returns a snapshot of events recorded since this capture was installed.
    pub fn events(&self) -> Vec<TransactionEvent> {
        self.writer.recorded_events()
    }
}
