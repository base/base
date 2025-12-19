//! Traits for the RPC module.

use alloy_primitives::TxHash;
use base_reth_relay::types::{EncryptedTransactionRequest, EncryptedTransactionResponse, RelayParameters};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

use crate::{Bundle, MeterBundleResponse, TransactionStatusResponse};

/// RPC API for transaction metering
#[rpc(server, namespace = "base")]
pub trait MeteringApi {
    /// Simulates and meters a bundle of transactions
    #[method(name = "meterBundle")]
    async fn meter_bundle(&self, bundle: Bundle) -> RpcResult<MeterBundleResponse>;
}

/// RPC API for transaction status
#[rpc(server, namespace = "base")]
pub trait TransactionStatusApi {
    /// Gets the status of a transaction
    #[method(name = "transactionStatus")]
    async fn transaction_status(&self, tx_hash: TxHash) -> RpcResult<TransactionStatusResponse>;
}

/// RPC API for encrypted transaction relay
#[rpc(server, namespace = "base")]
pub trait EncryptedRelayApi {
    /// Submits an encrypted transaction for relay to the sequencer.
    ///
    /// The transaction must be encrypted with the sequencer's current or previous
    /// encryption public key (from `getRelayParameters`) and include a valid
    /// proof-of-work nonce.
    #[method(name = "sendEncryptedTransaction")]
    async fn send_encrypted_transaction(
        &self,
        request: EncryptedTransactionRequest,
    ) -> RpcResult<EncryptedTransactionResponse>;

    /// Returns the current relay parameters.
    ///
    /// Includes the encryption public key, PoW difficulty, and other configuration
    /// needed to submit encrypted transactions.
    #[method(name = "getRelayParameters")]
    async fn get_relay_parameters(&self) -> RpcResult<RelayParameters>;
}
