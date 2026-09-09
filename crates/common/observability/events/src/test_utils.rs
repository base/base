//! Records globally emitted transaction events for tests.
//!
//! [`TransactionEventCapture::install`] registers an in-memory writer in the
//! process-global slot when it is empty, using a shared
//! [`TransactionEventRecorder`]. Each install clears that recorder and holds a
//! lock so concurrent capture tests do not interleave.

use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::{
    GlobalTransactionEventWriter, TransactionEvent, TransactionEventRecorder,
    TransactionEventWriter,
};

static TEST_CAPTURE_SERIAL: Mutex<()> = Mutex::new(());
static TEST_RECORDER: OnceLock<TransactionEventRecorder> = OnceLock::new();

/// Process-global capture of emitted transaction events for tests.
#[derive(Debug)]
pub struct TransactionEventCapture {
    _serial: MutexGuard<'static, ()>,
    recorder: TransactionEventRecorder,
}

impl TransactionEventCapture {
    /// Installs a process-global in-memory writer and starts a fresh recording window.
    pub fn install() -> Self {
        let serial = TEST_CAPTURE_SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        let recorder = TEST_RECORDER.get_or_init(TransactionEventRecorder::new).clone();
        let _ = GlobalTransactionEventWriter::get_or_init(TransactionEventWriter::in_memory(
            "test",
            recorder.clone(),
        ));
        recorder.clear();
        Self { _serial: serial, recorder }
    }

    /// Returns a snapshot of events recorded since this capture was installed.
    pub fn events(&self) -> Vec<TransactionEvent> {
        self.recorder.events()
    }
}
