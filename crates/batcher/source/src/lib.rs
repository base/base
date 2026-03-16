#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
pub use error::SourceError;

mod event;
pub use event::L2BlockEvent;

mod traits;
pub use traits::UnsafeBlockSource;

mod polling;
pub use polling::PollingSource;

mod subscription;
pub use subscription::BlockSubscription;

mod hybrid;
pub use hybrid::HybridBlockSource;

mod channel;
pub use channel::ChannelBlockSource;

mod l1_event;
pub use l1_event::L1HeadEvent;

mod l1_source;
pub use l1_source::L1HeadSource;

mod l1_polling;
pub use l1_polling::L1HeadPolling;

mod l1_subscription;
pub use l1_subscription::L1HeadSubscription;

mod l1_hybrid;
pub use l1_hybrid::HybridL1HeadSource;

mod l1_channel;
pub use l1_channel::ChannelL1HeadSource;

pub mod test_utils;
