//! Base network types.

use base_common_consensus::{BaseReceipt, OpTxType, ReceiptWithBloom};

use crate::Network;

/// Types for a Base chain network.
#[derive(Clone, Copy, Debug)]
pub struct Base {
    _private: (),
}

impl Network for Base {
    type TxType = OpTxType;

    type TxEnvelope = base_common_consensus::BaseTxEnvelope;

    type UnsignedTx = base_common_consensus::BaseTypedTransaction;

    type ReceiptEnvelope = ReceiptWithBloom<BaseReceipt>;

    type Header = base_common_consensus::Header;

    type TransactionRequest = base_common_rpc_types::BaseTransactionRequest;

    type TransactionResponse = base_common_rpc_types::BaseTransaction;

    type ReceiptResponse = base_common_rpc_types::BaseTransactionReceipt;

    type HeaderResponse = base_common_rpc_types::BaseHeaderResponse;

    type BlockResponse = base_common_rpc_types::BaseBlockResponse<Self::TransactionResponse>;
}
