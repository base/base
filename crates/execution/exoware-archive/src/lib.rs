#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
pub use error::ArchiveError;

mod keys;
pub use keys::{ArchiveKey, FORMAT_VERSION, Family, NUMBER_LEN, StateKey, StateKind};

mod offsets;
pub use offsets::{COMPACT_MAX, OffsetSet, TAG_COMPACT, TAG_ROARING};

mod config;
pub use config::{ArchiveConfig, CONFIG_LEN_WITH_RETENTION, MAX_BLOCKS_PER_SEGMENT};

mod marker;
pub use marker::{MARKER_LEN, SegmentDigest, SegmentMarker};

mod block;
pub use block::{
    ACCOUNT_CHANGE_LEN, AccountChange, AccountInfo, BlockBody, BlockCodec, BlockData,
    STORAGE_CHANGE_LEN, StorageChange,
};

mod verify;
pub use verify::BlockVerifier;

mod rows;
pub use rows::{RowBuilder, SegmentRows};

mod metrics;
pub use metrics::ArchiveMetrics;

mod store;
pub use store::{
    ArchiveStore, DEFAULT_NAMESPACE, DEFAULT_STORE_TIMEOUT, ExowareSession, ExowareStore,
    GET_MANY_BATCH, StoreSession,
};

mod archive;
pub use archive::{
    DEFAULT_PRUNE_GRACE_SEGMENTS, ExowareArchive, HighestMarker, PRUNE_CAPTURE, ProvisionOutcome,
};

mod source;
pub use source::{BlockSource, LocalAvailability, NodeState, SegmentLoader};
#[cfg(test)]
pub use source::{MockBlockSource, MockNodeState};

mod publisher;
pub use publisher::{
    DEFAULT_MAX_BATCH_BYTES, DEFAULT_MAX_BATCH_ROWS, MarkerState, PublishOutcome, PublisherOptions,
    PutSize, SegmentPublisher, StepOutcome,
};

mod reader;
pub use reader::{
    ArchiveReader, ArchiveRequest, ArchiveSnapshot, ArchivedBlock, ArchivedLog,
    ArchivedTransaction, DEFAULT_MAX_LOG_ALTERNATIVES, DEFAULT_MAX_LOG_CANDIDATES, LogFilter,
    ReaderLimits,
};

mod audit;
pub use audit::{AuditFinding, AuditReport, SegmentAuditor};

#[cfg(test)]
pub mod test_utils;
