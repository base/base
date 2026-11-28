#![doc = include_str!("../README.md")]

pub use base_common_genesis::BaseFeature;
// Shared with the base workspace's evm2 crate: fork activations, the block-boundary state
// transitions (Canyon, Denim's BaseTime, Everest) and the executor's error and context types.
pub use base_common_evm2::{
    BaseBlockExecutionCtx, BaseForkActivations, BaseSpecId, BaseTime, BaseTimeTransitionError, BlockGasLimitExceeded,
    Canyon, CumulativeGasOverflow, DaFootprintAboveGasLimit, Everest, IrregularStateChange, PreExecutionError,
};


mod chain;
pub use chain::ChainSpec;

mod l1block;
pub use l1block::L1BlockInfo;

mod validation;
pub use validation::{BlockRules, ComputedHeader, Invalid, MAXIMUM_EXTRA_DATA_SIZE, MAXIMUM_GAS_LIMIT};

mod block;
pub use block::{Transactions, assemble_header, computed_header, receipts_root};

mod transaction;
pub use transaction::BaseTxEnvelope;

mod handler;
pub use handler::BaseTxHandlerHooks;

mod precompiles;

mod ecrecover_cache;
pub use ecrecover_cache::install_ecrecover_cache;

mod native_precompiles;
pub use native_precompiles::{BasePrecompiles, CallEnv, EvmStorageProvider};

mod executor;
pub use executor::{BaseBlockExecutor, BlockEnvInput, BlockExecutionResult};

mod registry;

pub use base_common_base_tx::{IntrinsicGas, TxBaseGasSchedule};

mod nonce_manager;
pub use nonce_manager::{ExpiringNonceError, NonceManager, NonceOverflow};

mod base_tx;
pub use base_tx::{TX_CONTEXT_ADDRESS, TxBaseError, TxBaseExecutor, TxBaseSimulation};


mod types;
pub use types::{BaseEvmExt, BaseEvmTypes, BaseTxResultExt};
