//! Public submission lifecycle and manager-side completion channels.

use alloy_rpc_types_eth::TransactionReceipt;
use tokio::sync::{oneshot, watch};

use crate::{TxManagerError, TxManagerResult};

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
    /// Waiting for nonce assignment and transaction preparation.
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

/// Cloneable handle used to observe or await a transaction submission.
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

/// Manager-side sender for one submission's lifecycle updates.
#[derive(Debug)]
pub struct SubmissionTracker {
    /// Lifecycle snapshot sender retained until the submission resolves.
    status: watch::Sender<SubmissionSnapshot>,
}

impl SubmissionTracker {
    /// Creates the manager and caller sides of a submission lifecycle channel.
    pub fn channel(id: SubmissionId) -> (Self, SubmissionHandle) {
        let (status_tx, status_rx) = watch::channel(SubmissionSnapshot::staged(id));
        (Self { status: status_tx }, SubmissionHandle::new(status_rx))
    }

    /// Publishes a non-terminal lifecycle transition.
    pub fn update(&self, status: SubmissionStatus) {
        self.status.send_modify(|snapshot| snapshot.status = status);
    }

    /// Publishes the terminal outcome and closes the manager side.
    pub fn finish(self, outcome: SubmissionResult) {
        self.status.send_modify(|snapshot| {
            snapshot.status = SubmissionStatus::Resolved(Box::new(outcome));
        });
    }
}

/// Destination for the result of a normal submission or cancellation request.
#[derive(Debug)]
pub enum SubmissionCompletion {
    /// Normal submission observed through a [`SubmissionHandle`].
    Transaction(SubmissionTracker),
    /// Cancellation request waiting until its transaction may be live.
    Cancel(oneshot::Sender<TxManagerResult<()>>),
}

impl SubmissionCompletion {
    /// Sends the result using the semantics of the original request.
    pub fn finish(self, outcome: SubmissionResult, cancellation_confirmed: bool) {
        match self {
            Self::Transaction(tracker) => tracker.finish(outcome),
            Self::Cancel(result) => {
                let response = if cancellation_confirmed || outcome.is_ok() {
                    Ok(())
                } else {
                    Err(outcome.expect_err("non-success outcome contains error"))
                };

                let _ = result.send(response);
            }
        }
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
