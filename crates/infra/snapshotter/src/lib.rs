#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub use base_reth_cli::{
    ChunkFilename, ChunkedArchive, ComponentManifest, ManifestGenerationParams, OutputFileChecksum,
    ProgressDisplay, SingleArchive, SnapshotGenerator, SnapshotManifest, SnapshotManifestExt,
};

mod config;
pub use config::{DEFAULT_TIP_THRESHOLD_SECS, S3ConfigType, SnapshotterConfig};

mod progress;
pub use progress::UploadProgress;

mod container;
pub use container::{ContainerManager, DockerContainerManager};

mod tip;
pub use tip::{RpcTipChecker, TipChecker, TipStatus};

mod upload;
pub use upload::{
    SnapshotRun, SnapshotUploadParams, SnapshotUploader, StreamingMultipartUpload, UploadStrategy,
};

mod orchestrator;
pub use orchestrator::Snapshotter;
