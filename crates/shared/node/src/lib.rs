#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod builder;
pub use builder::{BaseBuilder, BaseRpcContext};

mod extension;
pub use extension::{BaseNodeExtension, FromExtensionConfig};

mod handle;
pub use handle::BaseNodeHandle;

mod runner;
pub use runner::BaseNodeRunner;

mod types;
pub use types::{BaseNodeBuilder, OpBuilder, OpProvider};

mod node;
pub use node::BaseNode;

mod add_ons;
pub use add_ons::{BaseAddOns, BaseAddOnsBuilder};

#[cfg(feature = "test-utils")]
pub mod test_utils;
