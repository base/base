//! RPC client abstractions and transaction submission.

mod client;
pub use client::{
    BaseFeeExt, BatchRpcClient, BatchSendError, BatchSendResult, JSON_RPC_METHOD_NOT_FOUND,
    MAX_BATCH_RPC_SIZE, QueryProvider, RPC_TIMEOUT, ReplyStr, RpcProviders, RpcResultExt,
    SubmitErrorReply, SubmitItem, SubmitParams, SubmitReply, SubmitRequest, TxpoolAdminClient,
    ValiditySidecar, WalletProvider, create_wallet_provider,
};
