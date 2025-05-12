use std::sync::Arc;

use op_succinct_host_utils::fetcher::OPSuccinctDataFetcher;

pub const RANGE_ELF_BUMP: &[u8] = include_bytes!("../../../elf/range-elf-bump");
pub const RANGE_ELF_EMBEDDED: &[u8] = include_bytes!("../../../elf/range-elf-embedded");
pub const AGGREGATION_ELF: &[u8] = include_bytes!("../../../elf/aggregation-elf");
pub const CELESTIA_RANGE_ELF_EMBEDDED: &[u8] =
    include_bytes!("../../../elf/celestia-range-elf-embedded");

// TODO: Update to EigenDA Range ELF Embedded
pub const EIGENDA_RANGE_ELF_EMBEDDED: &[u8] = include_bytes!("../../../elf/range-elf-embedded");

/// Get the range ELF depending on the feature flag.
pub fn get_range_elf_embedded() -> &'static [u8] {
    cfg_if::cfg_if! {
        if #[cfg(feature = "celestia")] {
            CELESTIA_RANGE_ELF_EMBEDDED
        } else {
            RANGE_ELF_EMBEDDED
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(feature = "celestia")] {
        use op_succinct_celestia_host_utils::host::CelestiaOPSuccinctHost;

        /// Initialize the Celestia host.
        pub fn initialize_host(
            fetcher: Arc<OPSuccinctDataFetcher>,
        ) -> Arc<CelestiaOPSuccinctHost> {
            tracing::info!("Initializing host with Celestia DA");
            Arc::new(CelestiaOPSuccinctHost::new(fetcher))
        }
    } else {
        use op_succinct_ethereum_host_utils::host::SingleChainOPSuccinctHost;

        /// Initialize the default (ETH-DA) host.
        pub fn initialize_host(
            fetcher: Arc<OPSuccinctDataFetcher>,
        ) -> Arc<SingleChainOPSuccinctHost> {
            tracing::info!("Initializing host with Ethereum DA");
            Arc::new(SingleChainOPSuccinctHost::new(fetcher))
        }
    }
}
