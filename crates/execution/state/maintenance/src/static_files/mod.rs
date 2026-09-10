//! Static-file production jobs.
mod segments;
pub use segments::{Receipts as StaticFileReceipts, Segment as StaticFileSegmentWriter};
mod static_file_producer;
pub use static_file_producer::{
    StaticFileProducer, StaticFileProducerInner, StaticFileProducerResult,
    StaticFileProducerWithResult,
};

pub use base_execution_state_types::*;
