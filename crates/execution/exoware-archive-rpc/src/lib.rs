#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod args;
pub use args::{DEFAULT_SEGMENT_BLOCKS, ExowareArchiveArgs};

mod source;
pub use source::{ArchiveProvider, ProviderSource};

mod convert;
pub use convert::{ArchiveConverter, PreparedBlock};

mod retention;
pub use retention::{LocalRetention, RetentionCache, StateDomain};

mod handle;
pub use handle::{ArchiveHandle, HashLookup, NodeArchiveReader};

mod eth;
pub use eth::{ArchiveEth, ArchiveEthApiServer, ArchiveRpcBoundary, LocalEthApi};

mod extension;
pub use extension::{ExowareArchiveExtension, RETENTION_TTL};
