//! Ethereum L1 chain configurations for known networks.

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
};

use alloy_eips::eip7840::BlobParams;

mod mainnet;
pub use mainnet::Mainnet;

mod sepolia;
pub use sepolia::Sepolia;

mod holesky;
pub use holesky::Holesky;

mod hoodi;
pub use hoodi::Hoodi;

mod config;
pub use config::L1_CONFIGS;

/// Shared blob schedule builder for all Ethereum networks.
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
            (
                alloy_hardforks::EthereumHardfork::Osaka.name().to_string().to_lowercase(),
                BlobParams::osaka(),
            ),
            (
                alloy_hardforks::EthereumHardfork::Bpo1.name().to_string().to_lowercase(),
                BlobParams::bpo1(),
            ),
            (
                alloy_hardforks::EthereumHardfork::Bpo2.name().to_string().to_lowercase(),
                BlobParams::bpo2(),
            ),
        ])
    }
}
