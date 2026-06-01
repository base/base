#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod utils;
pub use utils::unique_name;

mod b20;
pub use b20::{B20CreateConfig, B20PrecompileClient};

mod config;
pub use config::*;

mod containers;
pub use containers::*;

mod deployer;
pub use deployer::*;

mod docker;
pub use docker::*;

mod host;
pub use host::*;

mod images;
pub use images::*;

mod l1;
pub use l1::*;

mod l2;
pub use l2::*;

mod network;
pub use network::*;

mod rpc;
pub use rpc::*;

mod setup;
pub use setup::*;

mod smoke;
pub use smoke::{SystemTestStack, SystemTestStackBuilder};

mod system_config;
pub use system_config::*;

mod urls;
pub use urls::SystemTestUrls;
