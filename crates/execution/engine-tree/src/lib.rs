#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cached_execution;
pub use cached_execution::{
    CachedExecutionProvider, CachedExecutor, FlashblocksCachedExecutionProvider,
    NoopCachedExecutionProvider,
};

mod validator;
pub use validator::{BaseEngineValidator, BaseEngineValidatorBuilder};
