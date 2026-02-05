//! P2P peer information collector.

use crate::types::PeerData;

/// Collector for P2P peer information.
#[derive(Debug, Default)]
pub(crate) struct PeerCollector;

impl PeerCollector {
    /// Creates a new peer collector.
    pub(crate) const fn new() -> Self {
        Self
    }

    /// Collects peer data from counts (generates placeholder peers for pie chart).
    /// In a real implementation, this would receive actual peer information.
    pub(crate) fn collect(
        &self,
        connected: usize,
        _inbound: usize,
        _outbound: usize,
    ) -> Vec<PeerData> {
        // Generate placeholder peer entries for visualization.
        // In production, this would be populated with actual peer data.
        (0..connected)
            .map(|i| PeerData {
                contexts: 1,
                // Placeholder client type distribution
                client_type: ((i % 3) as u8),
                version: 67, // eth/67 protocol version
                head: 0,
            })
            .collect()
    }
}
