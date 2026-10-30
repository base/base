#![doc = include_str!("../README.md")]

mod storage;
pub use storage::{ReplayStore, StoreError};

mod writer;
pub use writer::ReplayWriter;

mod s3;
pub use s3::ReplayS3;

mod local;
pub use local::LocalPath;
