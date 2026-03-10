//! Transaction manager trait definitions.

use alloy_primitives::Address;
use alloy_rpc_types_eth::TransactionReceipt;
use tokio::sync::oneshot;

use crate::{TxCandidate, TxManagerError, TxManagerResult};

/// Result type returned by async send operations.
pub type SendResponse = TxManagerResult<TransactionReceipt>;

/// Lean public API for transaction management.
///
/// Callers only need [`send`](TxManager::send),
/// [`send_async`](TxManager::send_async), and
/// [`from_address`](TxManager::from_address).
/// Other accessors (chain ID, block number, etc.) are available
/// directly on [`SimpleTxManager`](crate::SimpleTxManager).
pub trait TxManager: Send + Sync {
    /// Sends a transaction and waits for its receipt.
    fn send(
        &self,
        candidate: TxCandidate,
    ) -> impl Future<Output = Result<TransactionReceipt, TxManagerError>> + Send;

    /// Sends a transaction asynchronously, returning a channel for the result.
    fn send_async(
        &self,
        candidate: TxCandidate,
    ) -> impl Future<Output = oneshot::Receiver<SendResponse>> + Send;

    /// Returns the address transactions are sent from.
    #[allow(clippy::wrong_self_convention)]
    fn from_address(&self) -> Address;
}
