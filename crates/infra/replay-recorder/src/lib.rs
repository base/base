#![doc = include_str!("../README.md")]

mod recorder;
pub use recorder::Recorder;

mod call;
pub use call::{RecordedCall, RecordedTransaction, RpcEnvelope, RpcId};
