#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod builder;
mod db;
mod types;

pub use builder::{AccountChangesBuilder, FlashblockAccessListBuilder};
pub use db::FBALBuilderDb;
pub use types::FlashblockAccessList;
