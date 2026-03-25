#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
pub use error::SafeDBError;

mod response;
pub use response::SafeHeadResponse;

mod traits;
pub use traits::{SafeDBReader, SafeHeadListener};

mod disabled;
pub use disabled::DisabledSafeDB;

mod db;
pub use db::SafeDB;
