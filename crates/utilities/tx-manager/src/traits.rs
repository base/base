//! Transaction manager trait definitions.

use std::{fmt::Debug, future::Future};

use alloy_primitives::Address;
use alloy_rpc_types_eth::TransactionReceipt;
use tokio::sync::watch;

use crate::{TxCandidate, TxManagerError, TxManagerResult};

/// Terminal result returned by [`SubmissionHandle::wait`].
pub type SubmissionResult = TxManagerResult<TransactionReceipt>;

/// Stable identifier assigned to one transaction submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubmissionId(u64);

impl SubmissionId {
    /// Creates an identifier from its numeric representation.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// Observable lifecycle state of a transaction submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionStatus {
    /// Waiting for nonce assignment and construction.
    Staged,
    /// Signed and present in the pending ledger.
    Pending {
        /// Assigned transaction nonce.
        nonce: u64,
        /// Current signed version within the nonce slot.
        version: u32,
    },
    /// The submission reached its terminal result.
    Resolved(
        /// Confirmed receipt or terminal transaction-manager error.
        Box<SubmissionResult>,
    ),
}

/// Observable snapshot of one transaction submission.
#[derive(Debug, Clone)]
pub struct SubmissionSnapshot {
    /// Stable submission identifier.
    pub id: SubmissionId,
    /// Current lifecycle status.
    pub status: SubmissionStatus,
}

impl SubmissionSnapshot {
    /// Creates the initial staged snapshot for a submission.
    pub const fn staged(id: SubmissionId) -> Self {
        Self { id, status: SubmissionStatus::Staged }
    }
}

/// Cloneable handle for observing and awaiting a transaction submission.
#[derive(Debug, Clone)]
pub struct SubmissionHandle {
    /// Receiver carrying the latest lifecycle snapshot.
    rx: watch::Receiver<SubmissionSnapshot>,
}

impl SubmissionHandle {
    /// Creates a handle over a submission lifecycle channel.
    pub const fn new(rx: watch::Receiver<SubmissionSnapshot>) -> Self {
        Self { rx }
    }

    /// Creates a handle that already contains a terminal outcome.
    pub fn resolved(outcome: SubmissionResult) -> Self {
        let (_tx, rx) = watch::channel(SubmissionSnapshot {
            id: SubmissionId::new(0),
            status: SubmissionStatus::Resolved(Box::new(outcome)),
        });
        Self::new(rx)
    }

    /// Returns the stable submission identifier.
    pub fn id(&self) -> SubmissionId {
        self.rx.borrow().id
    }

    /// Returns the latest lifecycle snapshot.
    pub fn snapshot(&self) -> SubmissionSnapshot {
        self.rx.borrow().clone()
    }

    /// Waits until the submission reaches a terminal outcome.
    pub async fn wait(mut self) -> SubmissionResult {
        loop {
            if let SubmissionStatus::Resolved(outcome) = self.rx.borrow_and_update().status.clone()
            {
                return *outcome;
            }
            if self.rx.changed().await.is_err() {
                return Err(TxManagerError::ChannelClosed);
            }
        }
    }
}

/// Lean public API for transaction management.
///
/// Callers submit a candidate, then observe or await the returned
/// [`SubmissionHandle`].
pub trait TxManager: Send + Sync + Debug {
    /// Enqueues a transaction and returns its lifecycle handle.
    ///
    /// Construction and publication are coordinated in the background. Each
    /// backend publishes in nonce order while independent backends, canonical
    /// confirmation, and fee replacement progress concurrently.
    fn submit(&self, candidate: TxCandidate) -> SubmissionHandle;

    /// Returns the address transactions are sent from.
    fn sender_address(&self) -> Address;

    /// Attempt to cancel a stuck txpool transaction by sending a self-transfer
    /// with a higher gas price at the same nonce, freeing the slot.
    ///
    /// A successful result means the cancellation transaction may be live.
    /// Canonical confirmation can still be pending.
    ///
    /// The default implementation is a no-op that immediately returns `Ok(())`,
    /// suitable for test managers and environments where txpool management is
    /// not needed.
    fn cancel_tx(&self) -> impl Future<Output = TxManagerResult<()>> + Send {
        std::future::ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TxManagerError, test_utils::StubReceipt};

    #[tokio::test]
    async fn submission_handle_yields_ok_on_success() {
        let receipt = StubReceipt::success();
        let result = SubmissionHandle::resolved(Ok(receipt.clone())).wait().await;
        assert_eq!(result.unwrap(), receipt);
    }

    #[tokio::test]
    async fn submission_handle_yields_inner_error() {
        let result = SubmissionHandle::resolved(Err(TxManagerError::NonceTooLow)).wait().await;
        assert_eq!(result.unwrap_err(), TxManagerError::NonceTooLow);
    }

    #[tokio::test]
    async fn submission_handle_maps_channel_closed() {
        let (tx, rx) = watch::channel(SubmissionSnapshot::staged(SubmissionId::new(1)));
        let handle = SubmissionHandle::new(rx);
        drop(tx);
        let result = handle.wait().await;
        assert_eq!(result.unwrap_err(), TxManagerError::ChannelClosed);
    }

    #[tokio::test]
    async fn submission_handle_observes_later_resolution() {
        let id = SubmissionId::new(1);
        let (status_tx, status_rx) = watch::channel(SubmissionSnapshot::staged(id));
        let handle = SubmissionHandle::new(status_rx);
        let receipt = StubReceipt::success();

        status_tx.send_replace(SubmissionSnapshot {
            id,
            status: SubmissionStatus::Resolved(Box::new(Ok(receipt.clone()))),
        });

        assert_eq!(handle.wait().await.unwrap(), receipt);
    }

    #[test]
    fn submission_handle_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<SubmissionHandle>();
    }
}
