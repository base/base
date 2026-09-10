//! Header, body, and file download algorithms for synchronization.

mod bodies;
#[cfg(any(test, feature = "test-utils"))]
pub use bodies::test_utils as body_downloads;
pub use bodies::{
    BODIES_TASK_BUFFER_SIZE, BodiesDownloader, BodiesDownloaderBuilder, BodyDownloadTask,
    NoopBodiesDownloader,
};

mod headers;
#[cfg(any(test, feature = "test-utils"))]
pub use headers::test_utils as header_downloads;
pub use headers::{
    HEADERS_TASK_BUFFER_SIZE, HeaderDownloadTask, NoopHeaderDownloader, ReverseHeadersDownloader,
    ReverseHeadersDownloaderBuilder, SyncTargetBlock,
};

mod metrics;
pub use metrics::*;

#[cfg(any(test, feature = "file-client"))]
mod file_client;
#[cfg(any(test, feature = "file-client"))]
pub use file_client::{
    ChunkedFileReader, DEFAULT_BYTE_LEN_CHUNK_CHAIN_FILE, DecodedFileChunk, FileClient,
    FileClientError, FromReader,
};
#[cfg(any(test, feature = "file-client"))]
mod receipt_file_client;
#[cfg(any(test, feature = "file-client"))]
pub use receipt_file_client::{
    FromReceiptReader, ReceiptDecoder, ReceiptFileClient, ReceiptWithBlockNumber,
};
#[cfg(any(test, feature = "file-client"))]
mod file_codec;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
