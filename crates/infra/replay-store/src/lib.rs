#![doc = include_str!("../README.md")]
#![cfg(unix)]

mod storage;
pub use storage::{ReplayStore, StoreError};

mod writer;
pub use writer::ReplayWriter;

mod s3;
pub use s3::ReplayS3;

mod local;
pub use local::{LocalObject, LocalTemp};
