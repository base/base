#![doc = include_str!("../README.md")]

mod storage;
pub use storage::{ReplayStore, StoreError};

mod writer;
pub use writer::ReplayWriter;

mod local;
pub use local::LocalPath;
