//! Proof execution driver.

mod errors;
pub use errors::{DriverError, DriverResult};

mod pipeline;
pub use pipeline::DriverPipeline;

mod executor;
pub use executor::Executor;

mod core;
pub use core::Driver;

mod cursor;
pub use cursor::PipelineCursor;

mod tip;
pub use tip::TipCursor;
