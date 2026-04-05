#[cfg(feature = "signer")]
use alloy_primitives::Bytes;
use alloy_primitives::{B256, U64};
#[cfg(feature = "signer")]
use alloy_rpc_types_eth::TransactionRequest;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

/// The admin namespace endpoints.
///
/// See: <https://github.com/ethereum-optimism/optimism/blob/c7ad0ebae5dca3bf8aa6f219367a95c15a15ae41/op-node/node/api.go#L28-L36>
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "admin"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "admin"))]
pub trait BaseAdminApi {
    /// Resets the derivation pipeline.
    #[method(name = "resetDerivationPipeline")]
    async fn admin_reset_derivation_pipeline(&self) -> RpcResult<()>;

    /// Starts the sequencer at the given block hash.
    #[method(name = "startSequencer")]
    async fn admin_start_sequencer(&self, block_hash: B256) -> RpcResult<()>;

    /// Stops the sequencer and returns the last sealed block hash.
    #[method(name = "stopSequencer")]
    async fn admin_stop_sequencer(&self) -> RpcResult<B256>;

    /// Returns whether the sequencer is currently active.
    #[method(name = "sequencerActive")]
    async fn admin_sequencer_active(&self) -> RpcResult<bool>;
}

/// Op API extension for controlling the miner.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "miner"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "miner"))]
pub trait MinerApiExt {
    /// Sets the maximum data availability size of any tx allowed in a block, and the total max l1
    /// data size of the block. 0 means no maximum.
    #[method(name = "setMaxDASize")]
    async fn set_max_da_size(&self, max_tx_size: U64, max_block_size: U64) -> RpcResult<bool>;

    /// Returns the current maximum data availability size limits as (`max_tx_size`, `max_block_size`).
    /// Returns 0 for either value when no limit is set.
    #[method(name = "getMaxDASize")]
    async fn get_max_da_size(&self) -> RpcResult<(U64, U64)>;

    /// Sets the gas limit for future blocks produced by the miner.
    #[method(name = "setGasLimit")]
    async fn set_gas_limit(&self, gas_limit: U64) -> RpcResult<bool>;
}

/// JSON-RPC interface for the `eth_signTransaction` endpoint.
#[cfg(feature = "signer")]
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "eth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "eth"))]
pub trait EthSignerApi {
    /// Signs a transaction and returns the RLP-encoded signed envelope.
    #[method(name = "signTransaction")]
    async fn sign_transaction(&self, tx: TransactionRequest) -> RpcResult<Bytes>;
}
