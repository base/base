//! Headers download algorithms and background tasks.

/// A Linear downloader implementation.
mod reverse_headers;
pub use reverse_headers::{
    ReverseHeadersDownloader, ReverseHeadersDownloaderBuilder, SyncTargetBlock,
};

/// A header downloader that does nothing. Useful to build unwind-only pipelines.
mod noop;
pub use noop::NoopHeaderDownloader;

/// A downloader implementation that spawns a downloader to a task
mod task;
pub use task::{HEADERS_TASK_BUFFER_SIZE, HeaderDownloadTask};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
