#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), allow(unused_crate_dependencies))]

mod config;
pub use config::RoutingConfig;

mod router;
pub use router::{BuilderUnavailableError, HealthState, MultiplexRouter, ResolveFuture};

mod service_builder;
pub use service_builder::MultiplexingServiceBuilder;
