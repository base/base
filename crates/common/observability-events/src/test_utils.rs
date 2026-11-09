//! Records globally emitted transaction events for tests.
//!
//! [`TransactionEventCapture::install`] registers an in-memory writer in the
//! process-global slot when it is empty, using a shared
//! [`TransactionEventRecorder`] behind a shared [`TransactionEventGate`]. Each
//! install clears that recorder, reopens the gate, and holds a lock so
//! concurrent capture tests do not interleave.

use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::{
    GlobalTransactionEventWriter, TransactionEvent, TransactionEventGate, TransactionEventRecorder,
    TransactionEventWriter,
};

static TEST_CAPTURE_SERIAL: Mutex<()> = Mutex::new(());
static TEST_RECORDER: OnceLock<TransactionEventRecorder> = OnceLock::new();
static TEST_GATE: OnceLock<TransactionEventGate> = OnceLock::new();

/// Process-global capture of emitted transaction events for tests.
#[derive(Debug)]
pub struct TransactionEventCapture {
    _serial: MutexGuard<'static, ()>,
    recorder: TransactionEventRecorder,
    gate: TransactionEventGate,
}

impl TransactionEventCapture {
    /// Installs a process-global in-memory writer and starts a fresh recording window with an
    /// open gate.
    pub fn install() -> Self {
        let serial = TEST_CAPTURE_SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        let recorder = TEST_RECORDER.get_or_init(TransactionEventRecorder::new).clone();
        let gate = TEST_GATE.get_or_init(TransactionEventGate::default).clone();
        let _ = GlobalTransactionEventWriter::get_or_init(
            TransactionEventWriter::in_memory("test", recorder.clone()).with_gate(gate.clone()),
        );
        recorder.clear();
        gate.set_open(true);
        Self { _serial: serial, recorder, gate }
    }

    /// Returns a snapshot of events recorded since this capture was installed.
    pub fn events(&self) -> Vec<TransactionEvent> {
        self.recorder.events()
    }

    /// Returns the gate in front of the captured writer, so a test can close process-global
    /// emission the way a non-leader producer does.
    pub const fn gate(&self) -> &TransactionEventGate {
        &self.gate
    }
}
