use alloc::sync::Arc;

use base_common_chains::Upgrades;
use base_common_consensus::{BaseReceipt, BaseTransactionSigned};
use base_common_evm2::{BaseBlockExecutionCtx, BaseEvmTypes};
use evm2::{Evm, ExecutionConfig, evm::DynDatabase};
use reth_evm::BlockExecutorFactory;

use crate::{BaseEvmEnv, BaseRethBlockExecutor};

/// Creates Base EVMs and block executors.
///
/// EVMs get Base's transaction registry and the precompile provider from
/// [`BaseEvmTypes::native_precompiles`]: the static set plus the stateful Beryl and Cobalt
/// precompiles, configured with the chain's activation registry admin.
#[derive(Debug)]
pub struct BaseBlockExecutorFactory<ChainSpec> {
    chain_spec: Arc<ChainSpec>,
}

impl<ChainSpec> BaseBlockExecutorFactory<ChainSpec> {
    /// Creates a factory for `chain_spec`.
    pub const fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self { chain_spec }
    }

    /// Returns the chain spec.
    pub const fn chain_spec(&self) -> &Arc<ChainSpec> {
        &self.chain_spec
    }
}

impl<ChainSpec> Clone for BaseBlockExecutorFactory<ChainSpec> {
    fn clone(&self) -> Self {
        Self { chain_spec: Arc::clone(&self.chain_spec) }
    }
}

impl<ChainSpec: Upgrades> BlockExecutorFactory for BaseBlockExecutorFactory<ChainSpec> {
    /// Base EVMs need no factory state beyond the environment.
    type EvmFactory = ();
    type EvmTypes = BaseEvmTypes;
    type Transaction = BaseTransactionSigned;
    type Receipt = BaseReceipt;
    type Evm<'a> = Evm<'a, BaseEvmTypes>;
    type EvmEnv = BaseEvmEnv;
    type ExecutionCtx<'a>
        = BaseBlockExecutionCtx
    where
        Self: 'a;
    type Executor<'a>
        = BaseRethBlockExecutor<'a, ChainSpec>
    where
        Self: 'a;

    fn create_executor<'a>(
        &'a self,
        evm: Self::Evm<'a>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a>
    where
        Self: 'a,
    {
        BaseRethBlockExecutor::new(evm, ctx, &self.chain_spec)
    }

    fn evm_factory(&self) -> &Self::EvmFactory {
        &()
    }

    fn evm_with_env<'a, DB>(&self, db: DB, evm_env: Self::EvmEnv) -> Self::Evm<'a>
    where
        DB: DynDatabase + 'a,
    {
        let spec = evm_env.spec;
        Evm::new_with_execution_config(
            ExecutionConfig::for_spec_and_version(spec, evm_env.version),
            spec,
            evm_env.block,
            BaseEvmTypes::tx_registry(),
            db,
            BaseEvmTypes::native_precompiles(spec, self.chain_spec.activation_admin_address()),
        )
    }
}
