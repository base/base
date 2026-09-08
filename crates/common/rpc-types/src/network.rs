use alloy_provider::fillers::{
    ChainIdFiller, GasFiller, JoinFill, NonceFiller, RecommendedFillers,
};
use base_common_consensus::{BaseReceipt, OpTxType, ReceiptWithBloom};
use base_common_network::Network;

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

    type TransactionRequest = crate::BaseTransactionRequest;

    type TransactionResponse = crate::Transaction;

    type ReceiptResponse = crate::BaseTransactionReceipt;

    type HeaderResponse = crate::BaseHeaderResponse;

    type BlockResponse = crate::BaseBlockResponse<Self::TransactionResponse>;
}

impl RecommendedFillers for Base {
    type RecommendedFillers = JoinFill<GasFiller, JoinFill<NonceFiller, ChainIdFiller>>;

    fn recommended_fillers() -> Self::RecommendedFillers {
        Default::default()
    }
}
