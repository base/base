#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod generator;
pub use generator::{
    ChunkFilename, DEFAULT_BLOCKS_PER_FILE, DirectoryArchiveSink, ManifestGenerationParams,
    ProgressDisplay, SnapshotArchiveSink, SnapshotArchiveWriter, SnapshotGenerator,
    SnapshotManifestExt,
};

mod manifest;
pub use manifest::{
    ChunkedArchive, ComponentManifest, OutputFileChecksum, SingleArchive, SnapshotManifest,
};
