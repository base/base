#![doc = include_str!("../README.md")]

mod spec;
pub use spec::BaseSpecId;

mod canyon;
pub use canyon::Canyon;

mod zenith;
pub use zenith::Zenith;

mod base_time;
pub use base_time::{BaseTime, BaseTimeTransitionError};

mod transaction;
pub use transaction::BaseTxEnvelope;

mod handler;
pub use handler::BaseTxHandlerHooks;

mod precompiles;

mod executor;
pub use executor::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BlockExecutionResult, BlockGasLimitExceeded,
    CumulativeGasOverflow, DaFootprintAboveGasLimit, PreExecutionError,
};

mod registry;

pub use base_common_eip8130::{
    AuthWireForm, Eip8130GasSchedule, IntrinsicGas, IntrinsicGasError, IntrinsicGasInput,
};

mod nonce_manager;
pub use nonce_manager::{NonceManager, NonceOverflow};

mod transition;
pub use transition::{BaseForkActivations, IrregularStateChange};

mod types;
pub use types::{BaseEvmExt, BaseEvmTypes, BaseTxResultExt};
