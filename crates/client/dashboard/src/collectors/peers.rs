//! P2P peer information collector.

use crate::types::PeerData;

/// Client type IDs matching Nethermind's `getNodeType()` function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
pub(crate) enum ClientType {
    /// Unknown client.
    Unknown = 0,
    /// Nethermind client.
    Nethermind = 1,
    /// Geth client.
    Geth = 2,
    /// Besu client.
    Besu = 3,
    /// Erigon client.
    Erigon = 4,
    /// Reth client.
    Reth = 5,
}

impl ClientType {
    /// Attempts to detect client type from client ID string.
    #[allow(dead_code)]
    pub(crate) fn from_client_id(client_id: &str) -> Self {
        let lower = client_id.to_lowercase();
        if lower.contains("nethermind") {
            Self::Nethermind
        } else if lower.contains("geth") || lower.contains("go-ethereum") {
            Self::Geth
        } else if lower.contains("besu") {
            Self::Besu
        } else if lower.contains("erigon") {
            Self::Erigon
        } else if lower.contains("reth") {
            Self::Reth
        } else {
            Self::Unknown
        }
    }
}

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
        // Generate placeholder peer entries for visualization
        // In production, this would be populated with actual peer data
        (0..connected)
            .map(|i| PeerData {
                contexts: 1,
                // Distribute clients for visualization purposes
                client_type: ((i % 6) as u8),
                version: 67, // eth/67 protocol version
                head: 0,
            })
            .collect()
    }

    /// Collects peer data from actual peer information.
    #[allow(dead_code)]
    pub(crate) fn collect_from_peers(&self, peers: &[(String, u64)]) -> Vec<PeerData> {
        peers
            .iter()
            .map(|(client_id, head)| PeerData {
                contexts: 1,
                client_type: ClientType::from_client_id(client_id) as u8,
                version: 67,
                head: *head,
            })
            .collect()
    }
}
