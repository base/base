use alloy_consensus::{Eip658Value, Receipt};
use alloy_evm::eth::receipt_builder::ReceiptBuilderCtx;
use base_common_consensus::{BaseReceipt, BaseTransactionSigned, OpTxType};
use base_common_evm::BaseReceiptBuilder;
use reth_evm::Evm;

/// A builder that operates on op-reth primitive types, specifically [`BaseTransactionSigned`] and
/// [`BaseReceipt`].
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BaseRethReceiptBuilder;

impl BaseReceiptBuilder for BaseRethReceiptBuilder {
    type Transaction = BaseTransactionSigned;
    type Receipt = BaseReceipt;

    fn build_receipt<'a, E: Evm>(
        &self,
        ctx: ReceiptBuilderCtx<'a, OpTxType, E>,
    ) -> Result<Self::Receipt, ReceiptBuilderCtx<'a, OpTxType, E>> {
        match ctx.tx_type {
            OpTxType::Deposit => Err(ctx),
            ty => {
                let receipt = Receipt {
                    // Success flag was added in `EIP-658: Embedding transaction status code in
                    // receipts`.
                    status: Eip658Value::Eip658(ctx.result.is_success()),
                    cumulative_gas_used: ctx.cumulative_gas_used,
                    logs: ctx.result.into_logs(),
                };

                Ok(match ty {
                    OpTxType::Legacy => BaseReceipt::Legacy(receipt),
                    OpTxType::Eip1559 => BaseReceipt::Eip1559(receipt),
                    OpTxType::Eip2930 => BaseReceipt::Eip2930(receipt),
                    OpTxType::Eip7702 => BaseReceipt::Eip7702(receipt),
                    OpTxType::Deposit => unreachable!(),
                })
            }
        }
    }

    fn build_deposit_receipt(&self, inner: base_common_consensus::DepositReceipt) -> Self::Receipt {
        BaseReceipt::Deposit(inner)
    }
}
