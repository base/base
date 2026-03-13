#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod spec_id;
pub use spec_id::{spec, spec_by_timestamp_after_bedrock};

mod evm;
pub use evm::OpEvm;

mod factory;
pub use factory::OpEvmFactory;

mod tx_env;
pub use tx_env::OpTxEnv;

mod ctx;
pub use ctx::OpBlockExecutionCtx;

mod error;
pub use error::OpBlockExecutionError;

mod receipt_builder;
pub use receipt_builder::{OpAlloyReceiptBuilder, OpReceiptBuilder};

mod canyon;

mod tx_result;
pub use tx_result::OpTxResult;

mod executor;
pub use executor::OpBlockExecutor;

mod executor_factory;
pub use executor_factory::OpBlockExecutorFactory;
