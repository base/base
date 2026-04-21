//! RPC client abstractions and transaction submission.

mod client;
pub use client::{
    BatchRpcClient, BatchSendResult, ReceiptProvider, RpcClient, WalletProvider,
    create_wallet_provider,
};
