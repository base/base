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

    type HeaderResponse = base_common_rpc_types::BaseHeaderResponse;

    type BlockResponse = base_common_rpc_types::BaseBlockResponse<Self::TransactionResponse>;
}

impl RecommendedFillers for Base {
    type RecommendedFillers = JoinFill<GasFiller, JoinFill<NonceFiller, ChainIdFiller>>;

    fn recommended_fillers() -> Self::RecommendedFillers {
        Default::default()
    }
}
