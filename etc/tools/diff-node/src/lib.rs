#![doc = include_str!("../README.md")]

mod rpc;
pub use rpc::{CoverageApiServer, CoverageRpc, CoverageRpcExtension, NodeCoverage};
