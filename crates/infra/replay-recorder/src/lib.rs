#![doc = include_str!("../README.md")]

mod call;
pub use call::{CaptureContext, RecordedCall, RecordedTransaction, RpcEnvelope, RpcId};
