//! Commonly used reth CLI commands.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod chainspec;
pub use chainspec::{ChainSpecParser, ChainSpecValueParser};

mod components;
pub use components::CliNodeComponents;

pub mod common;
pub mod config_cmd;
pub mod db;
pub use db::{
    AccountStorageCommand, ChecksumCommand, ChecksumRocksDbTable, ClearCommand, CopyCommand,
    DiffCommand, GetCommand, GetRocksDbTable, ListCommand, OutputFormat, PruneCheckpointSetArgs,
    PruneCheckpointsCommand, PruneModeArg, RepairTrieCommand, SegmentArg, SettingsCommand,
    StageArg, StageCheckpointSetArgs, StageCheckpointsCommand, StateCommand,
    StaticFileHeaderCommand, StatsCommand, checksum_rocksdb,
};
pub mod download;
pub use download::{SelectionPreset, SelectorOutput, run_selector};
pub mod dump_genesis;
pub mod import;
pub mod import_core;
pub mod init_cmd;
pub mod init_state;
pub mod p2p;
pub mod prune;
pub mod re_execute;
pub mod stage;
#[cfg(feature = "arbitrary")]
pub mod test_vectors;

#[cfg(test)]
pub mod test_utils;

mod snapshots;
pub use snapshots::{DEFAULT_DOWNLOAD_URL, Snapshots};
