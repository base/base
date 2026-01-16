#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod actor;
mod error;
mod handlers;
mod processor;
mod request;

pub use actor::{DirectEngineActor, DirectEngineActorError};
// Re-export core trait from engine-driver
pub use base_engine_driver::DirectEngineApi;
pub use error::DirectEngineProcessorError;
// Re-export key types from kona for convenience
pub use kona_node_service::{
    EngineActorRequest, EngineClientError, EngineClientResult, EngineContext, EngineError,
    NodeActor,
};
pub use processor::DirectEngineProcessor;
pub use request::{EngineProcessingRequest, RoutedRequest};
