//! Base network types.

use base_common_types_chain::{BaseReceipt, OpTxType, ReceiptWithBloom};

use crate::Network;

/// Types for a Base chain network.
#[derive(Clone, Copy, Debug)]
pub struct Base {
    _private: (),
}

impl Network for Base {
    type TxType = OpTxType;

    type TxEnvelope = base_common_types_chain::BaseTxEnvelope;

    type UnsignedTx = base_common_types_chain::BaseTypedTransaction;

    type ReceiptEnvelope = ReceiptWithBloom<BaseReceipt>;

    type Header = base_common_types_chain::Header;

    type TransactionRequest = base_common_types_rpc::BaseTransactionRequest;

    type TransactionResponse = base_common_types_rpc::BaseTransaction;

    type ReceiptResponse = base_common_types_rpc::BaseTransactionReceipt;

    type HeaderResponse = base_common_types_rpc::Header;

    type BlockResponse = base_common_types_rpc::BaseBlockResponse;
}
