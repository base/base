#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

// Logging
#[macro_use]
extern crate tracing;
// Used in tests
use base_consensus_genesis as _;

mod builder;
pub use builder::{Discv5Builder, LocalNode};

mod error;
pub use error::Discv5BuilderError;

mod driver;
pub use driver::Discv5Driver;

mod handler;
pub use handler::{Discv5Handler, HandlerRequest};

mod metrics;
pub use metrics::Metrics;
