#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod actor;
pub use actor::DirectEngineActor;

mod error;
pub use error::{EngineActorError, ProcessorError};

pub mod handlers;

mod processor;
pub use processor::DirectEngineProcessor;

mod request;
pub use request::{
    BuildRequest, EngineActorRequest, ProcessFinalizedL2BlockNumberRequest,
    ProcessSafeL2SignalRequest, ProcessUnsafeL2BlockRequest, ResetRequest, SealRequest,
};

mod state;
pub use state::EngineSyncState;
