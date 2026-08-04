//! L1 origin selection for the sequencer's build loop.
//!
//! The [`L1OriginSelector`] resolves the current sequencing origin inline and prepares the next one
//! in the background, publishing the selected origin (header + receipts) on a one-slot channel. The
//! [`PrefetchedChainProvider`] serves the attributes builder from that channel, falling back to a
//! bounded direct RPC on a miss, so steady-state block building issues no inline L1 I/O on the
//! sequencer's hot path.

mod prepared;
pub use prepared::{LinkedOrigin, PreparedL1Origin};

mod provider;
pub use provider::{DelayedL1OriginSelectorProvider, L1OriginSelectorProvider};

mod selector;
#[cfg(test)]
pub use selector::MockOriginSelector;
pub use selector::{L1OriginSelector, L1OriginSelectorError, OriginSelector};

mod prefetched_chain_provider;
pub use prefetched_chain_provider::{PrefetchedChainProvider, PrefetchedChainProviderError};
