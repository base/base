#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

pub mod builder;
pub use builder::BasePayloadBuilder;
pub mod config;
pub mod error;
pub mod payload;
pub use payload::{BaseBuiltPayload, BasePayloadBuilderAttributes};

mod parkable;
pub use parkable::{
    NonParkablePayloadTransactions, ParkableBestPayloadTransactions, ParkablePayloadTransactions,
    PayloadTransactionInvalidated,
};

mod predicate_loads;
pub use predicate_loads::{PredicateLoadTracker, PredicateReadRecorder};

mod traits;
pub use traits::*;
mod types;
pub use types::BasePayloadTypes;

mod validity;
pub use validity::{
    ParkedPredicateIndex, StateChangeEffects, ValidityPredicateEvaluation, ValidityPredicateKey,
};

pub mod validator;
pub use validator::BaseExecutionPayloadValidator;
