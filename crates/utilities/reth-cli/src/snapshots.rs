//! Reth snapshot and pruning-default initialization utilities.

use std::borrow::Cow;

use reth_cli_commands::download::DownloadDefaults;
use reth_node_core::args::DefaultPruningValues;
use reth_prune_types::PruneMode;

/// Must stay a bare host: reth derives `{root}/api/snapshots` from it and appends
/// its own path segments, so a chain path here produces 404 manifest URLs.
const SNAPSHOT_SOURCE_URL: &str = "https://chain.base.org";
const FULL_HISTORY_DISTANCE: u64 = 1_339_200;

/// Reth snapshot and pruning-default initialization for Base execution layer binaries.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Snapshots;

impl Snapshots {
    /// Snapshot sources advertised in `--help`, derived from one snapshot root.
    pub fn download_defaults() -> DownloadDefaults {
        DownloadDefaults::default().with_snapshot_source_url(SNAPSHOT_SOURCE_URL).with_snapshots(
            vec![
                Cow::Borrowed("https://mainnet-v2-snapshots.base.org (mainnet)"),
                Cow::Borrowed("https://sepolia-v2-snapshots.base.org (sepolia)"),
                Cow::Borrowed("https://zeronet-v2-snapshots.base.org (zeronet)"),
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
