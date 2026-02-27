#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod base;
mod cached_execution;

pub use base::BaseEngineValidator;
pub use cached_execution::{CachedExecutionProvider, CachedExecutor, NoopCachedExecutionProvider};
