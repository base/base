#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod init;

mod db_tool;
pub use db_tool::*;

mod etl;
pub use etl::{Collector, EtlFile, EtlIter};

mod pruning;
pub use pruning::*;

mod static_files;
pub use static_files::*;

mod static_file_config;
pub use static_file_config::{BlocksPerFileConfig, StaticFilesConfig};

mod snapshot_schema;
pub use snapshot_schema::{
    ChunkedArchive, ComponentManifest, ComponentSelection, OutputFileChecksum, SingleArchive,
    SnapshotArchive, SnapshotComponentType, SnapshotManifest, chunk_filename, generate_manifest,
};

mod snapshot_generator;
pub use snapshot_generator::{
    ChunkFilename, ManifestGenerationParams, ProgressDisplay, SnapshotGenerator,
};
