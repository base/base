//! Host providers.

use alloy_provider::RootProvider;
use base_alloy_network::Base;
use base_consensus_providers::{OnlineBeaconClient, OnlineBlobProvider};

/// The providers required for the host.
#[derive(Debug, Clone)]
pub struct HostProviders {
    /// The L1 EL provider.
    pub l1: RootProvider,
    /// The L1 beacon node provider.
    pub blobs: OnlineBlobProvider<OnlineBeaconClient>,
    /// The L2 EL provider.
    pub l2: RootProvider<Base>,
}
