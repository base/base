//! Contains the factory.

use alloy_consensus::{Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{
    EvmFactory, FromRecoveredTx, FromTxWithEncoded,
    block::{BlockExecutorFactory, StateDB},
};
use base_common_chains::{ChainUpgrades, Upgrades};
use revm::Inspector;

use crate::{
    AlloyReceiptBuilder, BaseBlockExecutionCtx, BaseBlockExecutor, BaseEvmFactory,
    BaseReceiptBuilder, BaseTxEnv, BaseTxResult,
};

/// Ethereum block executor factory.
#[derive(Debug, Clone, Default, Copy)]
pub struct BaseBlockExecutorFactory<
    R = AlloyReceiptBuilder,
    Spec = ChainUpgrades,
    EvmFactory = BaseEvmFactory,
> {
    /// Receipt builder.
    receipt_builder: R,
    /// Chain specification.
    spec: Spec,
    /// EVM factory.
    evm_factory: EvmFactory,
}

impl<R, Spec, EvmFactory> BaseBlockExecutorFactory<R, Spec, EvmFactory> {
    /// Creates a new [`BaseBlockExecutorFactory`] with the given spec, [`EvmFactory`], and
    /// [`BaseReceiptBuilder`].
    pub const fn new(receipt_builder: R, spec: Spec, evm_factory: EvmFactory) -> Self {
        Self { receipt_builder, spec, evm_factory }
    }

    /// Exposes the receipt builder.
    pub const fn receipt_builder(&self) -> &R {
        &self.receipt_builder
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

impl<R, Spec, EvmF> BlockExecutorFactory for BaseBlockExecutorFactory<R, Spec, EvmF>
where
    R: BaseReceiptBuilder<
            Transaction: Transaction + Encodable2718 + TransactionEnvelope<TxType: Send + 'static>,
            Receipt: TxReceipt,
        > + Clone,
    Spec: Upgrades + Clone,
    EvmF: EvmFactory<
        Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction> + BaseTxEnv,
    >,
    Self: 'static,
{
    type EvmFactory = EvmF;
    type ExecutionCtx<'a> = BaseBlockExecutionCtx;
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;
    type TxExecutionResult = BaseTxResult<
        <EvmF as EvmFactory>::HaltReason,
        <R::Transaction as TransactionEnvelope>::TxType,
    >;
    type Executor<'a, DB: StateDB, I: Inspector<EvmF::Context<DB>>> =
        BaseBlockExecutor<EvmF::Evm<DB, I>, R, Spec>;

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
        BaseBlockExecutor::new(evm, ctx, self.spec.clone(), self.receipt_builder.clone())
    }
}
