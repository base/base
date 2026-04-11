use core::convert::Infallible;

use base_alloy_consensus::{BaseReceipt, BaseTxEnvelope};
use reth_rpc_convert::{TryFromReceiptResponse, TryFromTransactionResponse};

use crate::Base;

impl TryFromTransactionResponse<Base> for BaseTxEnvelope {
    type Error = Infallible;

    fn from_transaction_response(
        transaction_response: base_common_rpc_types::Transaction,
    ) -> Result<Self, Self::Error> {
        Ok(transaction_response.inner.into_inner())
    }
}

impl TryFromReceiptResponse<Base> for BaseReceipt {
    type Error = Infallible;

    fn from_receipt_response(
        receipt_response: base_common_rpc_types::BaseTransactionReceipt,
    ) -> Result<Self, Self::Error> {
        Ok(receipt_response.inner.inner.into_components().0.map_logs(Into::into))
    }
}
