//! Reth snapshot and pruning-default initialization utilities.

use std::borrow::Cow;

use reth_cli_commands::download::DownloadDefaults;
use reth_node_core::args::DefaultPruningValues;
use reth_prune_types::PruneMode;

pub(crate) const DEFAULT_DOWNLOAD_URL: &str = "https://chain.base.org/8453";
const SNAPSHOT_API_URL: &str = "https://chain.base.org/api/snapshots";
const FULL_HISTORY_DISTANCE: u64 = 1_339_200;

/// Reth snapshot and pruning-default initialization for Base execution layer binaries.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Snapshots;

impl Snapshots {
    /// Initializes Reth's global snapshot download URLs and pruning defaults.
    ///
    /// This sets up the snapshot sources and makes the full preset retain approximately one month
    /// of bodies, receipts, and account and storage history.
    ///
    /// ### Panics
    ///
    /// Panics if the download URLs or pruning defaults were already initialized.
    pub fn init_snapshots() {
        let download_defaults = DownloadDefaults {
            available_snapshots: vec![
                Cow::Owned(format!("{DEFAULT_DOWNLOAD_URL} (mainnet)")),
                Cow::Borrowed("https://chain.base.org/84532 (sepolia)"),
                Cow::Borrowed("https://chain.base.org/763360 (zeronet)"),
            ],
            default_base_url: Cow::Borrowed(DEFAULT_DOWNLOAD_URL),
            default_chain_aware_base_url: None,
            snapshot_api_url: Cow::Borrowed(SNAPSHOT_API_URL),
            long_help: None,
        };

        download_defaults.try_init().expect("failed to initialize download URLs");

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
