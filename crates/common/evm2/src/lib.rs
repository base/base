#![doc = include_str!("../README.md")]

mod spec;
pub use spec::BaseSpecId;

mod transaction;
pub use transaction::BaseTxEnvelope;

mod handler;
pub use handler::BaseTxHandlerHooks;

mod executor;
pub use executor::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BlockExecutionResult, CumulativeGasOverflow,
    PreExecutionError,
};

mod registry;

mod types;
pub use types::{BaseEvmExt, BaseEvmTypes, BaseTxResultExt};
