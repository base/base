#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cursor;
pub use cursor::{MemCursor, MemCursorMut};

mod db;
pub use db::{MemDb, SharedStore, TableData};

mod tx;
pub use tx::{MemTx, MemTxMut};
