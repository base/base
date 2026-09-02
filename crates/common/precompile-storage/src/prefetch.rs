//! Fire-and-forget storage prefetch hints for native precompiles.
//!
//! Native precompiles know the exact storage slots an operation will touch
//! before executing it (they are derivable from calldata alone), while the
//! journaled read path resolves slots one at a time. On a state database
//! whose working set exceeds the page cache, each cold read costs hundreds of
//! microseconds of serial page faults; issuing the same reads concurrently
//! costs roughly one read's latency regardless of batch size.
//!
//! [`PrefetchHint::send`] forwards slot sets to a process-wide
//! [`StoragePrefetcher`] installed by the node at startup. Hints are purely a
//! page-cache warmer: prefetched values are discarded, the metered journaled
//! reads that follow are unchanged, and a hint that races its own journaled
//! read is deduplicated by the kernel (the read blocks on the in-flight page
//! I/O rather than repeating it). When no prefetcher is installed — tests,
//! tools, and `no_std` proof environments — hints are a no-op atomic load.

use alloc::sync::Arc;

use alloy_primitives::{Address, U256};
use revm::primitives::OnceLock;

/// Sink for storage prefetch hints, typically a pool of workers reading the
/// hinted slots through independent state-provider handles.
pub trait StoragePrefetcher: Send + Sync + core::fmt::Debug {
    /// Hints that the given storage slots of `address` are about to be read.
    ///
    /// Implementations must not block: this is called from the hot execution
    /// path, so backpressure has to be handled by dropping hints.
    fn prefetch(&self, address: Address, slots: &[U256]);
}

/// The process-wide prefetcher. Never uninstalled once set.
static PREFETCHER: OnceLock<Arc<dyn StoragePrefetcher>> = OnceLock::new();

/// Entry point for issuing storage prefetch hints.
#[derive(Debug, Clone, Copy)]
pub struct PrefetchHint;

impl PrefetchHint {
    /// Installs the process-wide prefetcher.
    ///
    /// The first install wins; returns `false` if a prefetcher was already
    /// installed.
    pub fn install(prefetcher: Arc<dyn StoragePrefetcher>) -> bool {
        PREFETCHER.set(prefetcher).is_ok()
    }

    /// Forwards a hint to the installed prefetcher, if any.
    pub fn send(address: Address, slots: &[U256]) {
        if let Some(prefetcher) = PREFETCHER.get() {
            prefetcher.prefetch(address, slots);
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
        calls: Mutex<Vec<(Address, Vec<U256>)>>,
    }

    impl StoragePrefetcher for RecordingPrefetcher {
        fn prefetch(&self, address: Address, slots: &[U256]) {
            self.calls.lock().unwrap().push((address, slots.to_vec()));
        }
    }

    #[test]
    fn send_forwards_to_installed_prefetcher() {
        let address = Address::repeat_byte(0xB2);
        let slots = [U256::from(11u64), U256::from(9u64)];

        // Before install: must not panic.
        PrefetchHint::send(address, &slots);

        let recorder = Arc::new(RecordingPrefetcher::default());
        assert!(PrefetchHint::install(recorder.clone()));

        PrefetchHint::send(address, &slots);
        assert_eq!(*recorder.calls.lock().unwrap(), vec![(address, slots.to_vec())],);

        // Second install loses; the original prefetcher keeps receiving.
        assert!(!PrefetchHint::install(Arc::new(RecordingPrefetcher::default())));
        PrefetchHint::send(address, &slots[..1]);
        assert_eq!(recorder.calls.lock().unwrap().len(), 2);
    }
}
