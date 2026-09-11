//! Fire-and-forget state prefetch hints for native precompiles.
//!
//! Native precompiles can know the storage slots an operation will touch before
//! executing it, while the journaled EVM read path resolves those reads one at
//! a time. [`PrefetchHint`] forwards slot batches to a process-wide
//! [`StatePrefetcher`] installed by the node at startup. Prefetched values are
//! discarded and the metered journaled reads remain unchanged.
//!
//! When no prefetcher is installed — tests, tools, and `no_std` proof
//! environments — sending a hint is a no-op atomic load.

use alloc::sync::Arc;

use alloy_primitives::{Address, U256};
use revm::primitives::OnceLock;

/// One storage slot a producer expects to read shortly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefetchRequest {
    /// The account whose storage will be read.
    pub address: Address,
    /// The storage slot key.
    pub slot: U256,
}

/// Sink for state prefetch hints.
pub trait StatePrefetcher: Send + Sync + core::fmt::Debug {
    /// Hints that the given state is about to be read.
    ///
    /// Implementations must not block because this is called from the hot
    /// execution path.
    fn prefetch(&self, requests: &[PrefetchRequest]);
}

/// The process-wide prefetcher. Never uninstalled once set.
static PREFETCHER: OnceLock<Arc<dyn StatePrefetcher>> = OnceLock::new();

/// Entry point for issuing state prefetch hints.
#[derive(Debug, Clone, Copy)]
pub struct PrefetchHint;

impl PrefetchHint {
    /// Installs the process-wide prefetcher.
    ///
    /// The first install wins; returns `false` if a prefetcher was already
    /// installed.
    pub fn install(prefetcher: Arc<dyn StatePrefetcher>) -> bool {
        PREFETCHER.set(prefetcher).is_ok()
    }

    /// Forwards storage-slot hints for one address if a prefetcher is installed.
    ///
    /// The slot set is produced lazily, so dispatch pays neither its derivation
    /// nor a heap allocation when prefetching is disabled. When enabled, slot
    /// keys and requests stay in fixed stack buffers.
    pub fn send_slots_with<const N: usize>(
        address: Address,
        slots: impl FnOnce() -> ([U256; N], usize),
    ) {
        const CHUNK: usize = 8;

        let Some(prefetcher) = PREFETCHER.get() else {
            return;
        };
        let (slots, slot_count) = slots();
        for chunk in slots[..slot_count.min(N)].chunks(CHUNK) {
            let mut requests = [PrefetchRequest { address, slot: U256::ZERO }; CHUNK];
            for (request, &slot) in requests.iter_mut().zip(chunk) {
                request.slot = slot;
            }
            prefetcher.prefetch(&requests[..chunk.len()]);
        }
    }
}

#[cfg(test)]
mod tests {
    //! The recording double below is hand-rolled rather than `automock`ed:
    //! the prefetcher under test lives in a process-global static that is
    //! never dropped, so mockall's drop-time expectation checking would never
    //! run. Recording into shared state and asserting from the test body
    //! sidesteps that.

    use core::cell::Cell;
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug, Default)]
    struct RecordingPrefetcher {
        calls: Mutex<Vec<Vec<PrefetchRequest>>>,
    }

    impl StatePrefetcher for RecordingPrefetcher {
        fn prefetch(&self, requests: &[PrefetchRequest]) {
            self.calls.lock().unwrap().push(requests.to_vec());
        }
    }

    #[test]
    fn send_forwards_to_installed_prefetcher() {
        let address = Address::repeat_byte(0xB2);
        let slots = [U256::from(11u64), U256::from(9u64)];

        let evaluated = Cell::new(false);
        PrefetchHint::send_slots_with(address, || {
            evaluated.set(true);
            (slots, slots.len())
        });
        assert!(!evaluated.get());

        let recorder = Arc::new(RecordingPrefetcher::default());
        let recorder_for_prefetcher = Arc::<RecordingPrefetcher>::clone(&recorder);
        let prefetcher: Arc<dyn StatePrefetcher> = recorder_for_prefetcher;
        assert!(PrefetchHint::install(prefetcher));

        PrefetchHint::send_slots_with(address, || (slots, slots.len()));
        assert_eq!(
            *recorder.calls.lock().unwrap(),
            vec![vec![
                PrefetchRequest { address, slot: slots[0] },
                PrefetchRequest { address, slot: slots[1] },
            ]],
        );

        assert!(!PrefetchHint::install(Arc::new(RecordingPrefetcher::default())));
        PrefetchHint::send_slots_with(address, || (slots, 1));
        assert_eq!(recorder.calls.lock().unwrap().len(), 2);
    }
}
