#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

/// Domain events tracked by tracex.
pub mod events;
mod exex;
mod tracker;

pub use crate::{
    events::{EventLog, Pool, TxEvent},
    exex::tracex_exex,
};
