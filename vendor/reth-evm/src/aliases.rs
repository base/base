//! Helper aliases when working with [`crate::BaseEvmConfig`] and the traits in this crate.

use alloy_evm::{Database, EvmEnv, block::BlockExecutorFor};
use revm::{Inspector, database::State, inspector::NoOpInspector};

/// Helper to access [`alloy_evm::EvmFactory`] for a given [`crate::BaseEvmConfig`].
pub type EvmFactoryFor = base_common_evm::BaseEvmFactory;

/// Helper to access [`alloy_evm::EvmFactory::Spec`] for a given [`crate::BaseEvmConfig`].
pub type SpecFor = base_common_evm::BaseSpecId;

/// Helper to access [`alloy_evm::EvmFactory::BlockEnv`] for a given [`crate::BaseEvmConfig`].
pub type BlockEnvFor = revm::context::BlockEnv;

/// Helper to access [`alloy_evm::EvmFactory::Evm`] for a given [`crate::BaseEvmConfig`].
pub type EvmFor<DB, I = NoOpInspector> = base_common_evm::BaseEvm<DB, I>;

/// Helper to access [`alloy_evm::EvmFactory::Error`] for a given [`crate::BaseEvmConfig`].
pub type EvmErrorFor<DB> =
    revm::context_interface::result::EVMError<DB, base_common_evm::BaseTransactionError>;

/// Helper to access [`alloy_evm::EvmFactory::Context`] for a given [`crate::BaseEvmConfig`].
pub type EvmContextFor<DB> = base_common_evm::BaseContext<DB>;

/// Helper to access [`alloy_evm::EvmFactory::HaltReason`] for a given [`crate::BaseEvmConfig`].
pub type HaltReasonFor = base_common_evm::BaseHaltReason;

/// Helper to access [`alloy_evm::EvmFactory::Tx`] for a given [`crate::BaseEvmConfig`].
pub type TxEnvFor = base_common_evm::BaseTransaction<revm::context::TxEnv>;

/// Helper to access [`alloy_evm::block::BlockExecutorFactory::ExecutionCtx`] for a given [`crate::BaseEvmConfig`].
pub type ExecutionCtxFor = base_common_evm::BaseBlockExecutionCtx;

/// Helper to access [`alloy_evm::block::BlockExecutor`] for a given [`crate::BaseEvmConfig`].
pub type BlockExecutorForEvm<'a, DB, I = NoOpInspector> =
    BlockExecutorFor<'a, crate::BaseExecutorFactory, &'a mut State<DB>, I>;

/// Type alias for [`EvmEnv`] for a given [`crate::BaseEvmConfig`].
pub type EvmEnvFor = EvmEnv<SpecFor, BlockEnvFor>;

/// Helper trait to bound [`Inspector`] for a [`crate::BaseEvmConfig`].
pub trait InspectorFor<DB: Database>: Inspector<EvmContextFor<DB>> {}
impl<T, DB> InspectorFor<DB> for T
where
    DB: Database,
    T: Inspector<EvmContextFor<DB>>,
{
}
