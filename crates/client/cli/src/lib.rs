#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;
pub use config::{ConfigError, L1ConfigFile, L2ConfigFile};

mod l1;
pub use l1::L1ClientArgs;

mod l2;
pub use l2::L2ClientArgs;

mod signer;
pub use signer::{SignerArgs, SignerArgsParseError};
