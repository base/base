#![doc = include_str!("../README.md")]

mod spec;
pub use spec::BaseSpecId;

mod canyon;
pub use canyon::Canyon;

mod everest;
pub use everest::Everest;

mod base_time;
pub use base_time::{BaseTime, BaseTimeTransitionError};

mod transaction;
pub use transaction::{BaseTxEnvelope, Eip8130Simulation, Eip8130SimulationMode, Eip8130Tx};

mod handler;
pub use handler::BaseTxHandlerHooks;

mod precompiles;

mod l1_block;
pub use l1_block::L1Block;

mod executor;
pub use executor::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseExecutedTx, BlockExecutionResult,
    BlockGasLimitExceeded, CumulativeGasOverflow, DaFootprintAboveGasLimit, PreExecutionError,
};

mod eip8130;
pub use eip8130::{Eip8130Handler, Eip8130Prepared, Eip8130Settlement};

mod registry;

pub use base_common_eip8130::{
    AuthWireForm, Eip8130GasSchedule, IntrinsicGas, IntrinsicGasError, IntrinsicGasInput,
};

mod nonce_manager;
pub use nonce_manager::{NonceManager, NonceOverflow};

mod transition;
pub use transition::{BaseBlockState, BaseForkActivations, IrregularStateChange};

mod types;
pub use types::{BaseEvmExt, BaseEvmTypes, BaseTxResultExt};

#[cfg(feature = "reth")]
mod reth;
