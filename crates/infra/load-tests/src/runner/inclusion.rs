//! Inclusion signals consumed by the pacing controller.

use std::time::Instant;

use super::BlockPulse;

/// Source that caused the pacing controller to reconsider mempool depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InclusionSource {
    /// Canonical block polling.
    Canonical,
    /// Timer fallback used when the canonical watcher produces no timely signal.
    Safety,
}

/// Compact inclusion signal consumed by the pacing controller.
#[derive(Debug, Clone, Copy)]
pub struct InclusionPulse {
    /// Inclusion source.
    pub source: InclusionSource,
    /// Time the source observed the inclusion update.
    pub observed_at: Instant,
    /// Canonical block metadata, when this came from block polling.
    pub canonical: Option<BlockPulse>,
}

impl InclusionPulse {
    /// Creates a canonical inclusion pulse.
    pub const fn canonical(block: BlockPulse) -> Self {
        Self {
            source: InclusionSource::Canonical,
            observed_at: block.observed_at,
            canonical: Some(block),
        }
    }

    /// Creates a timer-driven safety pulse.
    pub const fn safety(observed_at: Instant) -> Self {
        Self { source: InclusionSource::Safety, observed_at, canonical: None }
    }
}
