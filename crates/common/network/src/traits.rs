//! JSON-RPC trait definition for the `eth_signTransaction` endpoint.

use alloy_primitives::Bytes;
use base_common_rpc_types::TransactionRequest;
use jsonrpsee::proc_macros::rpc;

/// JSON-RPC interface for the `eth_signTransaction` endpoint.
#[rpc(client, namespace = "eth")]
pub trait EthSignerApi {
    /// Signs a transaction and returns the RLP-encoded signed envelope.
    #[method(name = "signTransaction")]
    async fn sign_transaction(&self, tx: TransactionRequest) -> jsonrpsee::core::RpcResult<Bytes>;
}
