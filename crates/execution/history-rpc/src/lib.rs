#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod export;
pub use export::{
    ArchiveApiImpl, ArchiveExportLimits, ArchiveExportProvider, DEFAULT_MAX_CONCURRENT_RANGES,
    EXPORTED_PRUNE_SEGMENTS, RangePermits,
};

mod export_extension;
pub use export_extension::{ArchiveExportConfig, ArchiveExportExtension};

mod convert;
pub use convert::{ArchiveConverter, ArchivedBlockWithReceipts};

mod eth;
pub use eth::{
    ArchiveEth, ArchiveEthApiServer, ArchiveHandle, ArchiveRpcBoundary, ArchiveSource, LocalEthApi,
    LocalFilterApi, LocalRetention, MAX_ARCHIVE_LOG_RANGE, StateDomain,
};

mod eth_extension;
pub use eth_extension::{ArchiveEthConfig, ArchiveEthExtension, DEFAULT_STARTUP_CHECK_TIMEOUT};
