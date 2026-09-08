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

    type TransactionRequest = alloy_rpc_types_eth::transaction::TransactionRequest;

    type TransactionResponse = alloy_rpc_types_eth::Transaction;

    type ReceiptResponse = alloy_rpc_types_eth::TransactionReceipt;

    type HeaderResponse = alloy_rpc_types_eth::Header;

    type BlockResponse = alloy_rpc_types_eth::Block;
}
