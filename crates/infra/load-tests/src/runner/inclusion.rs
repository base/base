//! Inclusion signals shared by canonical and flashblock watchers.

use std::time::Instant;

use super::BlockPulse;

/// Source that caused the pacing controller to reconsider mempool depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InclusionSource {
    /// Canonical block polling.
    Canonical,
    /// Builder flashblock broadcast.
    Flashblock,
    /// Timer fallback used when neither watcher produces a timely signal.
    Safety,
}

/// Compact inclusion signal consumed by the pacing controller.
#[derive(Debug, Clone, Copy)]
pub struct InclusionPulse {
    /// Inclusion source.
    pub source: InclusionSource,
    /// Time the source observed the inclusion update.
    pub observed_at: Instant,
    /// Gas newly released from local in-flight accounting.
    pub released_gas: u128,
    /// Canonical block metadata, when this came from block polling.
    pub canonical: Option<BlockPulse>,
}

impl InclusionPulse {
    /// Creates a canonical inclusion pulse.
    pub const fn canonical(block: BlockPulse, released_gas: u128) -> Self {
        Self {
            source: InclusionSource::Canonical,
            observed_at: block.observed_at,
            released_gas,
            canonical: Some(block),
        }
    }

    /// Creates a flashblock inclusion pulse.
    pub const fn flashblock(observed_at: Instant, released_gas: u128) -> Self {
        Self { source: InclusionSource::Flashblock, observed_at, released_gas, canonical: None }
    }

    /// Creates a timer-driven safety pulse.
    pub const fn safety(observed_at: Instant) -> Self {
        Self { source: InclusionSource::Safety, observed_at, released_gas: 0, canonical: None }
    }
}
