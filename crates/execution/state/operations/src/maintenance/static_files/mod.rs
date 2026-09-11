//! Static-file production jobs.
mod segments;
pub use segments::Receipts as StaticFileReceipts;
mod static_file_producer;
use base_execution_state_types::StaticFileProducerEvent;
pub use static_file_producer::{
    StaticFileProducer, StaticFileProducerInner, StaticFileProducerResult,
};
