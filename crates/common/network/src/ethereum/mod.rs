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
    type TxType = base_common_consensus::TxType;

    type TxEnvelope = base_common_consensus::TxEnvelope;

    type UnsignedTx = base_common_consensus::TypedTransaction;

    type ReceiptEnvelope = base_common_consensus::ReceiptEnvelope;

    type Header = base_common_consensus::Header;

    type TransactionRequest = base_common_rpc_types::transaction::TransactionRequest;

    type TransactionResponse = base_common_rpc_types::Transaction;

    type ReceiptResponse = base_common_rpc_types::TransactionReceipt;

    type HeaderResponse = base_common_rpc_types::Header;

    type BlockResponse = base_common_rpc_types::Block;
}
