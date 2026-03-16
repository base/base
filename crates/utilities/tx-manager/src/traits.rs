//! Transaction manager trait definitions.

use std::{
    fmt::Debug,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use alloy_primitives::Address;
use alloy_rpc_types_eth::TransactionReceipt;
use tokio::sync::oneshot;

use crate::{TxCandidate, TxManagerError, TxManagerResult};

/// Result type returned by async send operations.
pub type SendResponse = TxManagerResult<TransactionReceipt>;

/// Handle returned by [`TxManager::send_async`] that resolves to a [`SendResponse`].
///
/// Wraps a `oneshot::Receiver` and maps a closed channel (sender dropped
/// before delivering a result) into [`TxManagerError::ChannelClosed`],
/// eliminating the two-layer `Result<SendResponse, RecvError>` callers would
/// otherwise need to handle.
#[derive(Debug)]
pub struct SendHandle {
    rx: oneshot::Receiver<SendResponse>,
}

impl SendHandle {
    /// Creates a new `SendHandle` from a oneshot receiver.
    pub const fn new(rx: oneshot::Receiver<SendResponse>) -> Self {
        Self { rx }
    }
}

impl Future for SendHandle {
    type Output = SendResponse;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `oneshot::Receiver` is `Unpin`, so direct polling is safe.
        match Pin::new(&mut self.get_mut().rx).poll(cx) {
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(_)) => Poll::Ready(Err(TxManagerError::ChannelClosed)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Lean public API for transaction management.
///
/// Callers only need [`send`](TxManager::send),
/// [`send_async`](TxManager::send_async), and
/// [`sender_address`](TxManager::sender_address).
/// Other accessors (chain ID, block number, etc.) are available
/// directly on [`SimpleTxManager`](crate::SimpleTxManager).
pub trait TxManager: Send + Sync + Debug {
    /// Sends a transaction and waits for its receipt.
    fn send(&self, candidate: TxCandidate) -> impl Future<Output = SendResponse> + Send;

    /// Sends a transaction asynchronously, returning a [`SendHandle`] for the result.
    fn send_async(&self, candidate: TxCandidate) -> impl Future<Output = SendHandle> + Send;

    /// Returns the address transactions are sent from.
    fn sender_address(&self) -> Address;

    /// Attempt to cancel a stuck txpool transaction by sending a self-transfer
    /// with a higher gas price at the same nonce, freeing the slot.
    ///
    /// Called by the batch driver when it receives [`TxOutcome::TxpoolBlocked`]
    /// to clear the blocked slot so submission can resume.
    ///
    /// The default implementation is a no-op that immediately returns `Ok(())`,
    /// suitable for test managers and environments where txpool management is
    /// not needed.
    fn cancel_tx(&self) -> impl Future<Output = Result<(), TxManagerError>> + Send {
        async { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;
    use crate::{TxManagerError, test_utils::stub_receipt};

    #[tokio::test]
    async fn send_handle_yields_ok_on_success() {
        let (tx, rx) = oneshot::channel();
        let handle = SendHandle::new(rx);
        let receipt = stub_receipt();
        tx.send(Ok(receipt.clone())).unwrap();
        let result = handle.await;
        assert_eq!(result.unwrap(), receipt);
    }

    #[tokio::test]
    async fn send_handle_yields_inner_error() {
        let (tx, rx) = oneshot::channel();
        let handle = SendHandle::new(rx);
        tx.send(Err(TxManagerError::NonceTooLow)).unwrap();
        let result = handle.await;
        assert_eq!(result.unwrap_err(), TxManagerError::NonceTooLow);
    }

    #[tokio::test]
    async fn send_handle_maps_channel_closed() {
        let (tx, rx) = oneshot::channel::<SendResponse>();
        let handle = SendHandle::new(rx);
        drop(tx);
        let result = handle.await;
        assert_eq!(result.unwrap_err(), TxManagerError::ChannelClosed);
    }

    #[test]
    fn send_handle_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<SendHandle>();
    }
}
