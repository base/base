#![doc = include_str!("../README.md")]

pub use base_common_genesis::BaseFeature;

mod spec;
pub use spec::BaseSpecId;

mod chain;
pub use chain::ChainSpec;

mod l1block;
pub use l1block::L1BlockInfo;

mod validation;
pub use validation::{BlockRules, ComputedHeader, Invalid, MAXIMUM_EXTRA_DATA_SIZE, MAXIMUM_GAS_LIMIT};

mod block;
pub use block::{Transactions, assemble_header, computed_header, receipts_root};

mod canyon;
pub use canyon::Canyon;

mod everest;
pub use everest::Everest;

mod base_time;
pub use base_time::{BaseTime, BaseTimeTransitionError};

mod transaction;
pub use transaction::BaseTxEnvelope;

mod handler;
pub use handler::BaseTxHandlerHooks;

mod precompiles;

mod native_precompiles;
pub use native_precompiles::{BasePrecompiles, CallEnv, EvmStorageProvider};

mod executor;
pub use executor::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BlockEnvInput, BlockExecutionResult,
    BlockGasLimitExceeded, CumulativeGasOverflow, DaFootprintAboveGasLimit, PreExecutionError,
};

mod registry;

pub use base_common_base_tx::{IntrinsicGas, TxBaseGasSchedule};

mod nonce_manager;
pub use nonce_manager::{ExpiringNonceError, NonceManager, NonceOverflow};

mod base_tx;
pub use base_tx::{TX_CONTEXT_ADDRESS, TxBaseError, TxBaseExecutor, TxBaseSimulation};

mod transition;
pub use transition::{BaseForkActivations, IrregularStateChange};

mod types;
pub use types::{BaseEvmExt, BaseEvmTypes, BaseTxResultExt};
