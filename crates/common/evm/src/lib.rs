#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod spec_id;
pub use spec_id::{spec, spec_by_timestamp_after_bedrock};

mod evm;
pub use evm::BaseEvm;

mod factory;
pub use factory::BaseEvmFactory;

mod tx_env;
pub use tx_env::BaseTxEnv;

mod ctx;
pub use ctx::BaseBlockExecutionCtx;

mod error;
pub use error::BaseBlockExecutionError;

mod receipt_builder;
pub use receipt_builder::{AlloyReceiptBuilder, BaseReceiptBuilder};

mod canyon;
pub use canyon::ensure_create2_deployer;

mod executor;
pub use executor::{BaseBlockExecutor, BaseTxResult};

mod executor_factory;
pub use executor_factory::BaseBlockExecutorFactory;
