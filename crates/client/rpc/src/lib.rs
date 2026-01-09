#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod base;

mod eth;
pub use eth::{
    pubsub::{EthPubSub, EthPubSubApiServer},
    rpc::{EthApiExt, EthApiOverrideServer},
    types::{BaseSubscriptionKind, ExtendedSubscriptionKind},
};

mod metrics;
