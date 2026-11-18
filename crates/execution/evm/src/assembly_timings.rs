use std::{
    cell::Cell,
    time::{Duration, Instant},
};

// Reth calls the assembler synchronously inside BlockBuilder::finish. Scope the collector to
// that call so replay, RPC and validation don't report producer samples on the same thread.
scoped_tls::scoped_thread_local!(static CURRENT: Cell<AssemblyTimings>);

/// Header commitment timings for one native payload, absent for operations not performed.
#[derive(Clone, Copy, Debug, Default)]
pub struct AssemblyTimings {
    /// Ordered transaction trie root.
    pub transactions_root: Option<Duration>,
    /// Fork-specific receipt trie root.
    pub receipts_root: Option<Duration>,
    /// Aggregation of receipt logs into the header bloom.
    pub logs_bloom: Option<Duration>,
    /// L2-to-L1 message passer storage root, when active.
    pub message_root: Option<Duration>,
}

impl AssemblyTimings {
    /// Collects the timings from an assembler invoked synchronously by `finish`.
    pub fn capture<T>(finish: impl FnOnce() -> T) -> (T, Self) {
        let timings = Cell::new(Self::default());
        let result = CURRENT.set(&timings, finish);
        (result, timings.into_inner())
    }

    /// Times an operation only when a composing builder is collecting commitments.
    pub fn measure<T>(
        field: impl FnOnce(&mut Self) -> &mut Option<Duration>,
        work: impl FnOnce() -> T,
    ) -> T {
        if !CURRENT.is_set() {
            return work();
        }
        let start = Instant::now();
        let result = work();
        let elapsed = start.elapsed();
        CURRENT.with(|cell| {
            let mut timings = cell.get();
            *field(&mut timings) = Some(elapsed);
            cell.set(timings);
        });
        result
    }
}
