//! Base L1 chain configurations for known networks.
//!
//! These mirror the Ethereum L1 configurations but describe the Base networks acting as the
//! parent ("L1") chain of an L3. Base is an OP Stack chain, so the configs are post-merge from
//! genesis and pin the time-based execution-layer forks to the equivalent OP upgrade activations.

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
};

use alloy_eips::eip7840::BlobParams;

mod mainnet;
pub use mainnet::BaseMainnet;

mod sepolia;
pub use sepolia::BaseSepolia;

pub(super) mod config;

/// Shared blob schedule builder for all Base networks.
///
/// Base inherits Ethereum's blob parameters for the forks it has adopted: Cancun (via Ecotone)
/// and Prague (via Isthmus).
pub(super) struct BlobSchedule;

impl BlobSchedule {
    pub(super) fn schedule() -> BTreeMap<String, BlobParams> {
        BTreeMap::from([
            (
                alloy_hardforks::EthereumHardfork::Cancun.name().to_string().to_lowercase(),
                BlobParams::cancun(),
            ),
            (
                alloy_hardforks::EthereumHardfork::Prague.name().to_string().to_lowercase(),
                BlobParams::prague(),
            ),
        ])
    }
}
