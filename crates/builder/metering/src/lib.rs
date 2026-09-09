#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub use base_metering::{BaseApiExtServer, MeteringStoreExt};

mod store;
pub use store::{
    DEFAULT_METERING_STORE_MAX_CAPACITY, DEFAULT_METERING_STORE_TTL_SECS, MeteringStore,
};
