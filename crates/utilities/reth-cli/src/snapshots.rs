//! Reth snapshot and pruning-default initialization utilities.

use std::borrow::Cow;

use base_common_chains::ChainConfig;
use reth_cli_commands::download::DownloadDefaults;
use reth_node_core::args::DefaultPruningValues;
use reth_prune_types::PruneMode;

/// Must stay a bare host: reth derives `{root}/api/snapshots` from it and appends
/// its own path segments, so a chain path here produces 404 manifest URLs.
const SNAPSHOT_SOURCE_URL: &str = "https://chain.base.org";
const MAINNET_SNAPSHOT_URL: &str = "https://mainnet-v2-snapshots.base.org";
const SEPOLIA_SNAPSHOT_URL: &str = "https://sepolia-v2-snapshots.base.org";
const ZERONET_SNAPSHOT_URL: &str = "https://zeronet-v2-snapshots.base.org";
const FULL_HISTORY_DISTANCE: u64 = 1_339_200;

/// Reth snapshot and pruning-default initialization for Base execution layer binaries.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Snapshots;

impl Snapshots {
    /// Snapshot sources advertised in `--help`, each labelled with the `--chain` name
    /// that selects it.
    pub fn download_defaults() -> DownloadDefaults {
        DownloadDefaults::default().with_snapshot_source_url(SNAPSHOT_SOURCE_URL).with_snapshots(
            vec![
                Cow::Owned(format!(
                    "{MAINNET_SNAPSHOT_URL} (--chain {})",
                    ChainConfig::MAINNET_SELECTOR
                )),
                Cow::Owned(format!(
                    "{SEPOLIA_SNAPSHOT_URL} (--chain {})",
                    ChainConfig::SEPOLIA_SELECTOR
                )),
                Cow::Owned(format!(
                    "{ZERONET_SNAPSHOT_URL} (--chain {})",
                    ChainConfig::ZERONET_SELECTOR
                )),
            ],
        )
    }

    /// Initializes Reth's global snapshot download URLs and pruning defaults.
    ///
    /// This sets up the snapshot sources and makes the full preset retain approximately one month
    /// of bodies, receipts, and account and storage history.
    ///
    /// ### Panics
    ///
    /// Panics if the download URLs or pruning defaults were already initialized.
    pub fn init_snapshots() {
        Self::download_defaults().try_init().expect("failed to initialize download URLs");

        let mut pruning_defaults = DefaultPruningValues::default();
        pruning_defaults.full_prune_modes.bodies_history =
            Some(PruneMode::Distance(FULL_HISTORY_DISTANCE));
        pruning_defaults.full_prune_modes.receipts =
            Some(PruneMode::Distance(FULL_HISTORY_DISTANCE));
        pruning_defaults.full_prune_modes.account_history =
            Some(PruneMode::Distance(FULL_HISTORY_DISTANCE));
        pruning_defaults.full_prune_modes.storage_history =
            Some(PruneMode::Distance(FULL_HISTORY_DISTANCE));
        pruning_defaults.full_bodies_history_use_pre_merge = false;
        pruning_defaults.try_init().expect("failed to initialize pruning defaults");
    }
}

/// Initializes Reth's global snapshot download URLs and pruning defaults.
///
/// Use this in execution layer binaries (base-node-reth, base-builder) that need
/// Reth's global download URLs initialized for snapshot downloads
///
/// This macro must be called from the binary crate to capture the correct URLs.
#[macro_export]
macro_rules! init_snapshots {
    () => {
        $crate::Snapshots::init_snapshots()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_snapshot_sources_name_chains_the_parser_accepts() {
        for source in &Snapshots::download_defaults().available_snapshots {
            let (url, chain) = source
                .split_once(" (--chain ")
                .unwrap_or_else(|| panic!("snapshot source {source} is missing a --chain label"));
            let chain = chain.trim_end_matches(')');

            assert!(url.starts_with("https://"), "snapshot source {url} must be an https URL");
            assert!(
                ChainConfig::by_any_name(chain).is_some(),
                "advertised --chain {chain} is rejected by the Base chain parser"
            );
        }
    }

    #[test]
    fn snapshot_root_carries_no_chain_path() {
        let defaults = Snapshots::download_defaults();
        let host = defaults.default_base_url.trim_start_matches("https://");

        assert!(
            !host.contains('/'),
            "default base URL {} must be the snapshot root; a chain path makes reth \
             build 404 manifest URLs",
            defaults.default_base_url
        );
        assert_eq!(
            defaults.snapshot_api_url,
            format!("{}/api/snapshots", defaults.default_base_url)
        );
    }

    #[test]
    fn full_preset_retains_one_month_of_history() {
        Snapshots::init_snapshots();

        let defaults = DefaultPruningValues::get_global();
        let distance = Some(PruneMode::Distance(FULL_HISTORY_DISTANCE));
        assert_eq!(defaults.full_prune_modes.bodies_history, distance);
        assert_eq!(defaults.full_prune_modes.receipts, distance);
        assert_eq!(defaults.full_prune_modes.account_history, distance);
        assert_eq!(defaults.full_prune_modes.storage_history, distance);
        assert!(!defaults.full_bodies_history_use_pre_merge);
    }
}
