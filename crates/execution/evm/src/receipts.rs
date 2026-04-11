use alloy_consensus::{Eip658Value, Receipt};
use alloy_evm::eth::receipt_builder::ReceiptBuilderCtx;
use base_common_consensus::{BaseReceipt, BaseTransactionSigned, BaseTxType};
use base_common_evm::BaseReceiptBuilder;
use reth_evm::Evm;

/// A builder that operates on op-reth primitive types, specifically [`BaseTransactionSigned`] and
/// [`BaseReceipt`].
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct OpRethReceiptBuilder;

impl BaseReceiptBuilder for OpRethReceiptBuilder {
    type Transaction = BaseTransactionSigned;
    type Receipt = BaseReceipt;

    fn build_receipt<'a, E: Evm>(
        &self,
        ctx: ReceiptBuilderCtx<'a, BaseTxType, E>,
    ) -> Result<Self::Receipt, ReceiptBuilderCtx<'a, BaseTxType, E>> {
        match ctx.tx_type {
            BaseTxType::Deposit => Err(ctx),
            ty => {
                let receipt = Receipt {
                    // Success flag was added in `EIP-658: Embedding transaction status code in
                    // receipts`.
                    status: Eip658Value::Eip658(ctx.result.is_success()),
                    cumulative_gas_used: ctx.cumulative_gas_used,
                    logs: ctx.result.into_logs(),
                };

                Ok(match ty {
                    BaseTxType::Legacy => BaseReceipt::Legacy(receipt),
                    BaseTxType::Eip1559 => BaseReceipt::Eip1559(receipt),
                    BaseTxType::Eip2930 => BaseReceipt::Eip2930(receipt),
                    BaseTxType::Eip7702 => BaseReceipt::Eip7702(receipt),
                    BaseTxType::Deposit => unreachable!(),
                })
            }
        }
    }

    fn build_deposit_receipt(&self, inner: base_common_consensus::DepositReceipt) -> Self::Receipt {
        BaseReceipt::Deposit(inner)
    }
}
