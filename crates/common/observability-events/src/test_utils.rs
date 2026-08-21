//! In-memory transaction event capture for tests.
//!
//! Installing a capture registers an in-memory [`TransactionEventWriter`] as a
//! process-global overlay so production emission stays on the normal writer path.
//! The overlay is cleared when the capture is dropped. Installing a capture
//! serializes other capture-using tests so parallel crates do not interleave
//! recorded events.

use std::sync::{Mutex, MutexGuard};

use crate::{TransactionEvent, TransactionEventWriter};

static TEST_CAPTURE_SERIAL: Mutex<()> = Mutex::new(());
static TEST_WRITER_OVERLAY: Mutex<Option<TransactionEventWriter>> = Mutex::new(None);

/// Process-global capture of emitted transaction events for tests.
#[derive(Debug)]
pub struct TransactionEventCapture {
    _serial: MutexGuard<'static, ()>,
    writer: TransactionEventWriter,
}

impl TransactionEventCapture {
    /// Installs a process-global in-memory writer, replacing any previously recorded events.
    pub fn install() -> Self {
        let serial = Self::hold_serial();
        let writer = TransactionEventWriter::in_memory("test");
        *TEST_WRITER_OVERLAY.lock().unwrap_or_else(|err| err.into_inner()) = Some(writer.clone());
        Self { _serial: serial, writer }
    }

    /// Returns a snapshot of events recorded since this capture was installed.
    pub fn events(&self) -> Vec<TransactionEvent> {
        self.writer.recorded_events()
    }

    /// Returns the in-memory writer installed by an active capture.
    pub fn installed_writer() -> Option<TransactionEventWriter> {
        TEST_WRITER_OVERLAY.lock().unwrap_or_else(|err| err.into_inner()).clone()
    }

    /// Holds the capture serial lock without installing a writer.
    pub fn hold_serial() -> MutexGuard<'static, ()> {
        TEST_CAPTURE_SERIAL.lock().unwrap_or_else(|err| err.into_inner())
    }
}

impl Drop for TransactionEventCapture {
    fn drop(&mut self) {
        *TEST_WRITER_OVERLAY.lock().unwrap_or_else(|err| err.into_inner()) = None;
    }
}
