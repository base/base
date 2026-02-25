#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod utils;
pub use utils::unique_name;

pub mod config;
pub mod containers;
pub mod deployer;
pub mod devnet_config;
pub mod docker;
pub mod host;
pub mod images;
pub mod l1;
pub mod l2;
pub mod network;
pub mod rpc;
pub mod setup;
pub mod smoke;
pub mod urls;

pub use setup::{L1GenesisOutput, L2DeploymentOutput, SetupContainer};
pub use smoke::{Devnet, DevnetBuilder};
pub use urls::DevnetUrls;
