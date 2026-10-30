#![doc = include_str!("../README.md")]

mod storage;
pub use storage::{ReplayStore, StoreError};

mod writer;
pub use writer::ReplayWriter;
