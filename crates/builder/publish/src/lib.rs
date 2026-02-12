#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod metrics;
pub use metrics::{NoopPublisherMetrics, PublisherMetrics, PublishingMetrics};

mod broadcast;
pub use broadcast::BroadcastLoop;

mod listener;
pub use listener::Listener;

mod publisher;
pub use publisher::WebSocketPublisher;
