use core::convert::Infallible;

use base_alloy_consensus::{OpReceipt, OpTxEnvelope};
use reth_rpc_convert::{TryFromReceiptResponse, TryFromTransactionResponse};

use crate::Base;

impl TryFromTransactionResponse<Base> for OpTxEnvelope {
    type Error = Infallible;

    fn from_transaction_response(
        transaction_response: base_alloy_rpc_types::Transaction,
    ) -> Result<Self, Self::Error> {
        Ok(transaction_response.inner.into_inner())
    }
}

impl TryFromReceiptResponse<Base> for OpReceipt {
    type Error = Infallible;

    fn from_receipt_response(
        receipt_response: base_alloy_rpc_types::OpTransactionReceipt,
    ) -> Result<Self, Self::Error> {
        Ok(receipt_response.inner.inner.into_components().0.map_logs(Into::into))
    }
}
