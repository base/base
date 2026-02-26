//! Contains the error type for the network driver builder.

use base_consensus_disc::Discv5BuilderError;
use base_consensus_gossip::GossipDriverBuilderError;

/// An error from the [`crate::NetworkBuilder`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NetworkBuilderError {
    /// An error from building the gossip driver.
    #[error(transparent)]
    GossipDriverBuilder(#[from] GossipDriverBuilderError),
    /// An error from building the discv5 driver.
    #[error(transparent)]
    DiscoveryDriverBuilder(#[from] Discv5BuilderError),
}
