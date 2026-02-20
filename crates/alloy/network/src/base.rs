use alloy_consensus::ReceiptWithBloom;
use alloy_network::Network;
use alloy_provider::fillers::{
    ChainIdFiller, GasFiller, JoinFill, NonceFiller, RecommendedFillers,
};
use base_alloy_consensus::{OpReceipt, OpTxType};

/// Types for a Base chain network.
#[derive(Clone, Copy, Debug)]
pub struct Base {
    _private: (),
}

impl Network for Base {
    type TxType = OpTxType;

    type TxEnvelope = base_alloy_consensus::OpTxEnvelope;

    type UnsignedTx = base_alloy_consensus::OpTypedTransaction;

    type ReceiptEnvelope = ReceiptWithBloom<OpReceipt>;

    type Header = alloy_consensus::Header;

    type TransactionRequest = base_alloy_rpc_types::OpTransactionRequest;

    type TransactionResponse = base_alloy_rpc_types::Transaction;

    type ReceiptResponse = base_alloy_rpc_types::OpTransactionReceipt;

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
