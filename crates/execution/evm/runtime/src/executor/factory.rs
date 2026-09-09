//! Contains the factory.

use base_common_chain_config::{ChainUpgrades, Upgrades};
use base_common_types_chain::{BaseReceipt, BaseTxEnvelope, OpTxType};
use base_execution_evm_runtime::Inspector;
use base_execution_evm_runtime::{BlockExecutorFactory, EvmFactory, StateDB};

use crate::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseEvmFactory, BaseTransaction, BaseTxResult,
};

/// Ethereum block executor factory.
#[derive(Debug, Clone, Default, Copy)]
pub struct BaseBlockExecutorFactory<Spec = ChainUpgrades, EvmFactory = BaseEvmFactory> {
    /// Chain specification.
    spec: Spec,
    /// EVM factory.
    evm_factory: EvmFactory,
}

impl<Spec, EvmFactory> BaseBlockExecutorFactory<Spec, EvmFactory> {
    /// Creates a new [`BaseBlockExecutorFactory`] with the given spec and [`EvmFactory`].
    pub const fn new(spec: Spec, evm_factory: EvmFactory) -> Self {
        Self { spec, evm_factory }
    }

    /// Exposes the chain specification.
    pub const fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Exposes the EVM factory.
    pub const fn evm_factory(&self) -> &EvmFactory {
        &self.evm_factory
    }
}

impl<Spec, EvmF> BlockExecutorFactory for BaseBlockExecutorFactory<Spec, EvmF>
where
    Spec: Upgrades + Clone,
    EvmF: EvmFactory<Tx = BaseTransaction>,
    Self: 'static,
{
    type EvmFactory = EvmF;
    type ExecutionCtx<'a> = BaseBlockExecutionCtx;
    type Transaction = BaseTxEnvelope;
    type Receipt = BaseReceipt;
    type TxExecutionResult = BaseTxResult<<EvmF as EvmFactory>::HaltReason, OpTxType>;
    type Executor<'a, DB: StateDB, I: Inspector<EvmF::Context<DB>>> =
        BaseBlockExecutor<EvmF::Evm<DB, I>, Spec>;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: EvmF::Evm<DB, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<EvmF::Context<DB>>,
    {
        BaseBlockExecutor::new(evm, ctx, self.spec.clone())
    }
}
