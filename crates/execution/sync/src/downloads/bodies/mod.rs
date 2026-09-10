//! Bodies download algorithms and background tasks.

/// A naive concurrent downloader.
#[expect(clippy::module_inception)]
mod bodies;
pub use bodies::{BodiesDownloader, BodiesDownloaderBuilder};

/// A body downloader that does nothing. Useful to build unwind-only pipelines.
mod noop;
pub use noop::NoopBodiesDownloader;

/// A downloader implementation that spawns a downloader to a task
mod task;
pub use task::{BODIES_TASK_BUFFER_SIZE, BodyDownloadTask};

mod queue;
mod request;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
