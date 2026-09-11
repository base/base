#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod report;
pub use report::{
    ClientMeta, Heads, IpSource, NODE_REPORT_SCHEMA_VERSION, NodeLayer, NodeReport,
    NodeReportEvent, NodeRole,
};

mod hardware;
pub use hardware::{Hardware, HardwarePlatform};

mod network;
pub use network::NetworkName;

mod config;
pub use config::{NodeConfigReport, PruneMode};

mod net;
pub use net::NetHealth;
