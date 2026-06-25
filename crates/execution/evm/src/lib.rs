#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

extern crate alloc;

mod build;
pub use build::BaseBlockAssembler;

mod config;
pub use config::{BaseEvmConfig, BaseExecutorProvider, BaseNextBlockEnvAttributes};

mod env;
pub use env::BaseEvmEnvBuilder;

mod error;
pub use error::{BaseBlockExecutionError, L1BlockInfoError};

mod l1;
pub use l1::*;

mod receipts;
pub use receipts::*;
