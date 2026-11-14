#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
pub use error::HistoryStoreError;

mod types;
pub use types::{ArchiveBlock, BlockData, BlockRows, BodyIndices};

mod layout;
pub use layout::ArchiveLayout;

mod keys;
pub use keys::{
    ACCOUNT_KIND, BLOCK_TAG, HISTORY_TAG, HashedKey, IndexKey, MANIFEST_TAG, STORAGE_KIND, TX_TAG,
    WATERMARK_KEY,
};

mod catalog;
pub use catalog::{
    ARCHIVE_FORMAT, Catalog, CatalogStore, RetiredSegment, SegmentSpan, VersionedCatalog,
};

mod segment;
pub use segment::{
    BodyIndicesFile, BuiltFile, BuiltSegment, ManifestEntry, SegmentBuild, SegmentBuilder,
    SegmentFile, SegmentFiles, SegmentManifest, SegmentTip,
};

mod verify;
pub use verify::{ChainTip, ChunkVerifier, VerifyError};

mod index;
pub use index::{IndexOptions, IndexReader, IndexWriter, SegmentIndex};

mod rpc;
pub use rpc::{
    ArchiveApiClient, ArchiveApiServer, ArchiveRpcError, ArchiveStatus, DEFAULT_MAX_RANGE_BLOCKS,
    DEFAULT_MAX_RANGE_BYTES, RangeBounds,
};

mod source;
#[cfg(test)]
pub use source::MockChainSource;
pub use source::{
    AuthHttpClient, ChainSource, DEFAULT_MAX_RESPONSE_BYTES, PoolStatus, RpcChainSource, SourcePool,
};

mod store;
pub use store::{ArchiveStore, StoreOptions};

mod objects;
pub use objects::{DEFAULT_UPLOAD_PART_SIZE, SegmentObjects, UploadReport};

mod writer;
pub use writer::{
    ArchiveWriter, ChainIdentity, PrepareOutcome, PreparedSegment, StepOutcome, WriterConfig,
};

mod gc;
pub use gc::{DEFAULT_ORPHAN_MIN_AGE, GcReport, ORPHAN_WINDOW, OrphanGc};

mod pruner;
pub use pruner::{DEFAULT_MAX_SEGMENTS_PER_PASS, DEFAULT_RETIREMENT_GRACE, Pruner};

mod service;
pub use service::{ArchiveService, MaintenanceConfig, MaintenanceReport};

mod router;
pub use router::{ArchiveSnapshot, RangeSplit, Route};

mod cache;
pub use cache::{
    CacheConfig, DEFAULT_MAX_CONCURRENT_FILLS, DiskLease, DiskState, DiskTier, ExistingEntry,
    FillShare, LoadedSegment, Reservation, SegmentCache, SegmentDir, SegmentVerification,
};

mod reader;
pub use reader::{
    ArchiveReader, ArchivedBlock, ArchivedBlockLogs, ArchivedLog, DEFAULT_SNAPSHOT_TTL,
    HistoryLookup, ReaderConfig,
};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
