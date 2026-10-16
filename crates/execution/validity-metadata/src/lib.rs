#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod metrics;
pub use metrics::ValidityMetadataMetrics;

mod record;
pub use record::{
    MAX_VALIDITY_RECORD_LEN, MAX_VALIDITY_TRANSACTIONS_PER_RECORD, VALIDITY_RECORD_HEADER_LEN,
    VALIDITY_RECORD_VERSION, ValidityBlockRecord, ValidityRecordError,
};

mod store;
pub use store::{ValidityBatchOutcome, ValidityMetadataError, ValidityMetadataStore};

mod recorder;
pub use recorder::{
    DEFAULT_VALIDITY_METADATA_QUEUE_CAPACITY, MAX_VALIDITY_METADATA_WRITE_BATCH,
    VALIDITY_METADATA_WRITER_THREAD, ValidityMetadataRecorder, ValidityRecordStatus,
};

#[cfg(test)]
pub mod test_utils;
