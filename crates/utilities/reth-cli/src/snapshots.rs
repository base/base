//! Reth snapshots initialization utilities

use std::borrow::Cow;

use reth_cli_commands::download::DownloadDefaults;

pub(crate) const DEFAULT_DOWNLOAD_URL: &str = "https://v2-snapshots-ui.vercel.app/8453";
const SNAPSHOT_API_URL: &str = "https://v2-snapshots-ui.vercel.app/api/snapshots";

/// Reth snapshot download URLs initialization for Base execution layer binaries
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Snapshots;

impl Snapshots {
    /// Initializes Reth's global download URLs for snapshots
    ///
    /// This sets up the available snapshots, base download URLs, and
    /// the snapshot API URL that Reth uses for picking which exact
    /// uploaded snapshot manifest to follow and download
    ///
    /// ### Panics
    ///
    /// Panics if unable to initialize download URLs.
    pub fn init_snapshots() {
        let download_defaults = DownloadDefaults {
            available_snapshots: vec![
                Cow::Owned(format!("{DEFAULT_DOWNLOAD_URL} (mainnet)")),
                Cow::Borrowed("https://v2-snapshots-ui.vercel.app/84532 (sepolia)"),
                Cow::Borrowed("https://v2-snapshots-ui.vercel.app/763360 (zeronet)"),
            ],
            default_base_url: Cow::Borrowed(DEFAULT_DOWNLOAD_URL),
            default_chain_aware_base_url: None,
            snapshot_api_url: Cow::Borrowed(SNAPSHOT_API_URL),
            long_help: None,
        };

        download_defaults.try_init().expect("failed to initialize download URLs");
    }
}

/// Initializes Reth's global download URLs for snapshots
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
