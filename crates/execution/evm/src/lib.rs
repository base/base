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
pub use env::{BaseEvmEnv, BaseEvmEnvBuilder};

mod error;
pub use error::{BaseBlockExecutionError, BaseInvalidTxError, L1BlockInfoError};

mod executor;
pub use executor::{BaseRethBlockExecutor, BaseRethBlockState, UpgradeActivations};

mod factory;
pub use factory::BaseBlockExecutorFactory;

#[cfg(feature = "rpc")]
mod forecast;
#[cfg(feature = "rpc")]
pub use forecast::BasePendingForecast;

mod l1;
pub use l1::*;
