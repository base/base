#![doc = include_str!("../README.md")]

mod record;
pub use record::{REPLAY_RECORD_FORMAT, ReplayRecord, ReplayRecordError, ReplayStream};
