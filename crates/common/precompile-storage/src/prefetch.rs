//! Fire-and-forget state prefetch hints for native precompiles.
//!
//! Native precompiles know the exact state an operation will touch before
//! executing it (it is derivable from calldata alone), while the journaled
//! read path resolves reads one at a time. On a state database whose working
//! set exceeds the page cache, each cold read costs hundreds of microseconds
//! of serial page faults; issuing the same reads concurrently costs roughly
//! one read's latency regardless of batch size.
//!
//! [`PrefetchHint::send`] forwards [`PrefetchRequest`] batches to a
//! process-wide [`StatePrefetcher`] installed by the node at startup. Hints
//! are purely a page-cache warmer: prefetched values are discarded, the
//! metered journaled reads that follow are unchanged, and a hint that races
//! its own journaled read is deduplicated by the kernel (the read blocks on
//! the in-flight page I/O rather than repeating it). When no prefetcher is
//! installed — tests, tools, and `no_std` proof environments — hints are a
//! no-op atomic load.

use alloc::{sync::Arc, vec::Vec};

use alloy_primitives::{Address, U256};
use revm::primitives::OnceLock;

/// One unit of state a producer expects to read shortly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefetchRequest {
    /// A storage slot of `address`.
    Slot {
        /// The account whose storage will be read.
        address: Address,
        /// The storage slot key.
        slot: U256,
    },
    /// The basic account info (balance, nonce, code hash) of `address`.
    Account {
        /// The account that will be loaded.
        address: Address,
    },
    /// The bytecode of `address`, warming both the account and its code.
    Code {
        /// The account whose bytecode will be loaded.
        address: Address,
    },
}

/// Sink for state prefetch hints, typically a pool of workers reading the
/// hinted state through independent state-provider handles.
pub trait StatePrefetcher: Send + Sync + core::fmt::Debug {
    /// Hints that the given state is about to be read.
    ///
    /// Implementations must not block: this is called from the hot execution
    /// path, so backpressure has to be handled by dropping hints.
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

    /// Forwards a hint batch to the installed prefetcher, if any.
    pub fn send(requests: &[PrefetchRequest]) {
        if let Some(prefetcher) = PREFETCHER.get() {
            prefetcher.prefetch(requests);
        }
    }

    /// Forwards a hint for storage slots of a single address, if a
    /// prefetcher is installed.
    ///
    /// The slot set is produced lazily so the hot dispatch path pays nothing
    /// — not even the closure's allocations — when no prefetcher is
    /// installed. Requests convert through a fixed stack buffer; batches
    /// larger than the buffer arrive as multiple hints.
    pub fn send_slots_with(address: Address, slots: impl FnOnce() -> Vec<U256>) {
        const CHUNK: usize = 8;
        let Some(prefetcher) = PREFETCHER.get() else {
            return;
        };
        for chunk in slots().chunks(CHUNK) {
            let mut requests = [PrefetchRequest::Slot { address, slot: U256::ZERO }; CHUNK];
            for (request, &slot) in requests.iter_mut().zip(chunk) {
                *request = PrefetchRequest::Slot { address, slot };
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

        // Before install: must not panic, and must not evaluate the closure.
        PrefetchHint::send_slots_with(address, || unreachable!("no prefetcher installed"));
        PrefetchHint::send(&[PrefetchRequest::Account { address }]);

        let recorder = Arc::new(RecordingPrefetcher::default());
        assert!(PrefetchHint::install(recorder.clone()));

        PrefetchHint::send_slots_with(address, || slots.to_vec());
        assert_eq!(
            *recorder.calls.lock().unwrap(),
            vec![vec![
                PrefetchRequest::Slot { address, slot: slots[0] },
                PrefetchRequest::Slot { address, slot: slots[1] },
            ]],
        );

        PrefetchHint::send(&[PrefetchRequest::Code { address }]);
        assert_eq!(
            recorder.calls.lock().unwrap().last().unwrap(),
            &vec![PrefetchRequest::Code { address }],
        );

        // Second install loses; the original prefetcher keeps receiving.
        assert!(!PrefetchHint::install(Arc::new(RecordingPrefetcher::default())));
        PrefetchHint::send_slots_with(address, || slots[..1].to_vec());
        assert_eq!(recorder.calls.lock().unwrap().len(), 3);
    }
}
