#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod registry;
pub use registry::Registry;

mod l1;
pub use l1::{L1Config, L1GenesisGetterErrors};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
