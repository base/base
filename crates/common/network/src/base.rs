use alloy_consensus::ReceiptWithBloom;
use alloy_network::Network;
use alloy_provider::fillers::{
    ChainIdFiller, GasFiller, JoinFill, NonceFiller, RecommendedFillers,
};
use base_common_consensus::{BaseReceipt, OpTxType};

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

    type Header = alloy_consensus::Header;

    type TransactionRequest = base_common_rpc_types::BaseTransactionRequest;

    type TransactionResponse = base_common_rpc_types::Transaction;

    type ReceiptResponse = base_common_rpc_types::BaseTransactionReceipt;

    type HeaderResponse = alloy_rpc_types_eth::Header;

    type BlockResponse =
        alloy_rpc_types_eth::Block<Self::TransactionResponse, Self::HeaderResponse>;
}

impl RecommendedFillers for Base {
    type RecommendedFillers = JoinFill<GasFiller, JoinFill<NonceFiller, ChainIdFiller>>;

    fn recommended_fillers() -> Self::RecommendedFillers {
        Default::default()
    }
}
