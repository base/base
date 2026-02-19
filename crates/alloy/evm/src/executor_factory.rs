use alloy_consensus::{Transaction, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{
    Database, EvmFactory, FromRecoveredTx, FromTxWithEncoded,
    block::{BlockExecutorFactory, BlockExecutorFor},
};
use base_alloy_hardforks::{OpChainHardforks, OpHardforks};
use revm::{Inspector, database::State};

use crate::{
    OpAlloyReceiptBuilder, OpBlockExecutionCtx, OpBlockExecutor, OpEvmFactory, OpReceiptBuilder,
    OpTxEnv,
};

/// Ethereum block executor factory.
#[derive(Debug, Clone, Default, Copy)]
pub struct OpBlockExecutorFactory<
    R = OpAlloyReceiptBuilder,
    Spec = OpChainHardforks,
    EvmFactory = OpEvmFactory,
> {
    /// Receipt builder.
    receipt_builder: R,
    /// Chain specification.
    spec: Spec,
    /// EVM factory.
    evm_factory: EvmFactory,
}

impl<R, Spec, EvmFactory> OpBlockExecutorFactory<R, Spec, EvmFactory> {
    /// Creates a new [`OpBlockExecutorFactory`] with the given spec, [`EvmFactory`], and
    /// [`OpReceiptBuilder`].
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

impl<R, Spec, EvmF> BlockExecutorFactory for OpBlockExecutorFactory<R, Spec, EvmF>
where
    R: OpReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt>,
    Spec: OpHardforks,
    EvmF: EvmFactory<
        Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction> + OpTxEnv,
    >,
    Self: 'static,
{
    type EvmFactory = EvmF;
    type ExecutionCtx<'a> = OpBlockExecutionCtx;
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: EvmF::Evm<&'a mut State<DB>, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: Database + 'a,
        I: Inspector<EvmF::Context<&'a mut State<DB>>> + 'a,
    {
        OpBlockExecutor::new(evm, ctx, &self.spec, &self.receipt_builder)
    }
}
