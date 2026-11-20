#![doc = include_str!("../README.md")]

pub use base_common_genesis::BaseFeature;

mod spec;
pub use spec::BaseSpecId;

mod canyon;
pub use canyon::Canyon;

mod everest;
pub use everest::Everest;

mod base_time;
pub use base_time::{BaseTime, BaseTimeTransitionError};

mod transaction;
pub use transaction::{BaseTxEnvelope, TxBaseEnv, TxBaseSimulation, TxBaseSimulationMode};

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

mod base_tx;
pub use base_tx::{TxBaseHandler, TxBasePrepared, TxBaseSettlement};

mod registry;

pub use base_common_base_tx::{IntrinsicGas, TxBaseGasSchedule};

mod nonce_manager;
pub use nonce_manager::{NonceManager, NonceOverflow};

mod transition;
pub use transition::{BaseBlockState, BaseForkActivations, IrregularStateChange};

mod types;
pub use types::{BaseEvmExt, BaseEvmTypes, BaseTxResultExt, MeteredGasEvmTypes, MeteredGasReport};

#[cfg(feature = "reth")]
mod reth;
