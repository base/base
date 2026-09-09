//! Ethereum L1 network and wallet integration.

use crate::Network;

mod builder;

mod wallet;
pub use wallet::{EthereumWallet, IntoWallet};

/// Types for a mainnet-like Ethereum network.
#[derive(Clone, Copy, Debug)]
pub struct Ethereum {
    _private: (),
}

impl Network for Ethereum {
    type TxType = base_common_types_chain::TxType;

    type TxEnvelope = base_common_types_chain::TxEnvelope;

    type UnsignedTx = base_common_types_chain::TypedTransaction;

    type ReceiptEnvelope = base_common_types_chain::ReceiptEnvelope;

    type Header = base_common_types_chain::Header;

    type TransactionRequest = base_common_types_rpc::transaction::TransactionRequest;

    type TransactionResponse = base_common_types_rpc::Transaction;

    type ReceiptResponse = base_common_types_rpc::TransactionReceipt;

    type HeaderResponse = base_common_types_rpc::Header;

    type BlockResponse = base_common_types_rpc::Block;
}
