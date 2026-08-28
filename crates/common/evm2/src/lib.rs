#![doc = include_str!("../README.md")]

mod spec;
pub use spec::BaseSpecId;

mod transaction;
pub use transaction::{BaseTxEnvelope, DEPOSIT_TX_TYPE, TxDeposit};

mod handler;
pub use handler::BaseTxHandlerHooks;

mod executor;
pub use executor::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BlockExecutionResult, CumulativeGasOverflow,
};

mod registry;

mod types;
pub use types::{BaseEvmExt, BaseEvmTypes, BaseTxResultExt};
