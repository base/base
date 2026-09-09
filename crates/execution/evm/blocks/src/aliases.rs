//! Helper aliases when working with [`crate::BaseEvmConfig`] and the traits in this crate.

use base_execution_evm_runtime::{BlockExecutorFor, Database, EvmEnv, NoOpInspector};
use base_execution_evm_runtime::{Inspector, database::State};

/// Helper to access [`base_execution_evm_runtime::EvmFactory`] for a given [`crate::BaseEvmConfig`].
pub type EvmFactoryFor = base_execution_evm_runtime::BaseEvmFactory;

/// Helper to access [`base_execution_evm_runtime::EvmFactory::Spec`] for a given [`crate::BaseEvmConfig`].
pub type SpecFor = base_execution_evm_runtime::BaseSpecId;

/// Helper to access [`base_execution_evm_runtime::EvmFactory::BlockEnv`] for a given [`crate::BaseEvmConfig`].
pub type BlockEnvFor = base_execution_evm_machine::BlockEnv;

/// Helper to access [`base_execution_evm_runtime::EvmFactory::Evm`] for a given [`crate::BaseEvmConfig`].
pub type EvmFor<DB, I = NoOpInspector> = base_execution_evm_runtime::BaseEvm<DB, I>;

/// Helper to access [`base_execution_evm_runtime::EvmFactory::Error`] for a given [`crate::BaseEvmConfig`].
pub type EvmErrorFor<DB> =
    base_execution_evm_machine::EVMError<DB, base_execution_evm_runtime::BaseTransactionError>;

/// Helper to access [`base_execution_evm_runtime::EvmFactory::Context`] for a given [`crate::BaseEvmConfig`].
pub type EvmContextFor<DB> = base_execution_evm_runtime::BaseContext<DB>;

/// Helper to access [`base_execution_evm_runtime::EvmFactory::HaltReason`] for a given [`crate::BaseEvmConfig`].
pub type HaltReasonFor = base_execution_evm_runtime::BaseHaltReason;

/// Helper to access [`base_execution_evm_runtime::EvmFactory::Tx`] for a given [`crate::BaseEvmConfig`].
pub type TxEnvFor = base_execution_evm_runtime::BaseTransaction;

/// Helper to access [`base_execution_evm_runtime::BlockExecutorFactory::ExecutionCtx`] for a given [`crate::BaseEvmConfig`].
pub type ExecutionCtxFor = base_execution_evm_runtime::BaseBlockExecutionCtx;

/// Helper to access [`base_execution_evm_runtime::BlockExecutor`] for a given [`crate::BaseEvmConfig`].
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
