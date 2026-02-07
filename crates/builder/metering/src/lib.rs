#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod ext;
pub use ext::{BaseApiExtServer, MeteringStoreExt};

mod extension;
pub use extension::MeteringStoreExtension;

mod store;
pub use store::MeteringStore;
