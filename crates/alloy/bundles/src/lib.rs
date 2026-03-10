#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod accepted;
pub use accepted::AcceptedBundle;

mod bundle;
pub use bundle::Bundle;

mod cancel;
pub use cancel::{BundleHash, CancelBundle};

mod meter;
pub use meter::{MeterBundleResponse, TransactionResult};

mod parsed;
pub use parsed::ParsedBundle;

mod traits;
pub use traits::{BundleExtensions, BundleTxs};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
