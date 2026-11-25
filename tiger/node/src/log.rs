//! Log lines written to stderr by a background thread, for the informational lines the Engine
//! API path prints for every block (the validated block, the new head). A write to a log file can
//! stall for milliseconds (a ZFS pool throttles writers while it flushes, such as during a
//! persistence batch), and those lines are printed before the response, some under the chain
//! lock. Errors, invalid blocks, retries and the lines before an exit stay on `eprintln!`, so
//! they are written before the caller goes on.
//!
//! Queued lines keep their order among themselves, but a line printed directly can land before a
//! queued line logged earlier. The queue is bounded: while the writer is stalled and the queue is
//! full, lines are dropped and counted, and the writer reports the count before its next line.

use std::{
    io::Write,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

/// Most lines queued; beyond it, lines are dropped.
const QUEUED_LINES: usize = 4096;

/// A queued line, or a request to confirm every line queued before it is written.
enum Entry {
    Line(String),
    Flush(mpsc::Sender<()>),
}

/// The background log writer.
#[derive(Debug, Clone, Copy)]
pub struct Log;

/// Lines dropped because the queue was full, not yet reported.
static DROPPED: AtomicU64 = AtomicU64::new(0);

impl Log {
    /// The queue, with its writer thread started on first use (`None` when the thread could not
    /// start: lines are then written by the caller).
    fn queue() -> Option<&'static mpsc::SyncSender<Entry>> {
        static QUEUE: OnceLock<Option<mpsc::SyncSender<Entry>>> = OnceLock::new();
        QUEUE
            .get_or_init(|| {
                let (sender, receiver) = mpsc::sync_channel::<Entry>(QUEUED_LINES);
                let write = move || {
                    for entry in receiver {
                        match entry {
                            Entry::Line(line) => {
                                let mut stderr = std::io::stderr().lock();
                                let dropped = DROPPED.swap(0, Ordering::Relaxed);
                                if dropped > 0 {
                                    let _ = writeln!(stderr, "tiger: log queue full, dropped {dropped} lines");
                                }
                                let _ = writeln!(stderr, "{line}");
                            }
                            Entry::Flush(done) => {
                                let _ = done.send(());
                            }
                        }
                    }
                };
                thread::Builder::new().name("log".into()).spawn(write).ok().map(|_| sender)
            })
            .as_ref()
    }

    /// Queues `line` for stderr, or drops it (counted) when the queue is full.
    pub fn line(line: String) {
        let Some(queue) = Self::queue() else {
            eprintln!("{line}");
            return;
        };
        match queue.try_send(Entry::Line(line)) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::TrySendError::Disconnected(Entry::Line(line))) => eprintln!("{line}"),
            Err(mpsc::TrySendError::Disconnected(Entry::Flush(_))) => {}
        }
    }

    /// Waits until every line queued so far is written, as before an exit.
    pub fn flush() {
        let Some(queue) = Self::queue() else { return };
        let (done, written) = mpsc::channel();
        if queue.send(Entry::Flush(done)).is_ok() {
            let _ = written.recv();
        }
        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            eprintln!("tiger: log queue full, dropped {dropped} lines");
        }
    }
}
