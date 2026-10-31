#![doc = include_str!("../README.md")]

mod record;
pub use record::{
    REPLAY_RECORD_FORMAT, ReplayRecord, ReplayRecordError, ReplayStream,
    SUPPORTED_REPLAY_RECORD_FORMATS,
};

mod frame;
pub use frame::ReplayFrame;
