#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

#[macro_use]
extern crate tracing;

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
