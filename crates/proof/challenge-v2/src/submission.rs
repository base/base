//! Submission task: drains [`Submission`]s sent through a
//! [`SubmissionHandle`] and submits each one through a single
//! [`TxManager`] for nonce safety, returning the per-transaction
//! outcome to the caller via a oneshot.

use std::fmt;

use alloy_primitives::{Address, Bytes, TxHash};
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
use derive_more::Debug;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::{BondRequest, DisputeRequest};

/// One unit of L1 work routed through [`SubmissionTask`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submission {
    /// Dispute action produced by a game worker.
    Dispute(DisputeRequest),
    /// Bond lifecycle action produced by a bond worker.
    Bond(BondRequest),
}

impl Submission {
    /// Game proxy targeted by this submission.
    pub const fn game_address(&self) -> Address {
        match self {
            Self::Dispute(r) => r.game_address,
            Self::Bond(r) => r.game_address,
        }
    }

    /// Encoded L1 calldata for this submission.
    pub fn into_calldata(self) -> Bytes {
        match self {
            Self::Dispute(r) => r.action.to_calldata(r.proof_bytes),
            Self::Bond(r) => r.action.to_calldata(),
        }
    }
}

impl fmt::Display for Submission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dispute(r) => fmt::Display::fmt(&r.action, f),
            Self::Bond(r) => fmt::Display::fmt(&r.action, f),
        }
    }
}

/// Errors returned by [`SubmissionHandle::submit`].
#[derive(Debug, Error)]
pub enum SubmitError {
    /// The transaction was included in a block with `status == 0`.
    #[error("transaction reverted on-chain (tx_hash {0})")]
    Reverted(TxHash),
    /// Underlying [`TxManager`] failure.
    #[error("tx manager error: {0}")]
    TxManager(#[from] TxManagerError),
    /// Submission channel closed.
    #[error("submission channel closed")]
    ChannelClosed,
}

/// Sender-side handle for [`SubmissionTask`].
#[derive(Debug, Clone)]
pub struct SubmissionHandle {
    sender: mpsc::Sender<Envelope>,
}

impl SubmissionHandle {
    /// Submits `submission` and awaits its on-chain outcome.
    pub async fn submit(&self, submission: Submission) -> Result<TxHash, SubmitError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender
            .send(Envelope { submission, response_tx })
            .await
            .map_err(|_| SubmitError::ChannelClosed)?;
        response_rx.await.map_err(|_| SubmitError::ChannelClosed)?
    }
}

/// One submission paired with the oneshot used to return its outcome.
struct Envelope {
    submission: Submission,
    response_tx: oneshot::Sender<Result<TxHash, SubmitError>>,
}

/// Long-running task that drains submissions and submits them
/// serially through a single [`TxManager`] (one sender, contiguous
/// nonces).
#[derive(Debug)]
pub struct SubmissionTask<Tx> {
    #[debug(skip)]
    tx_manager: Tx,
    #[debug(skip)]
    receiver: mpsc::Receiver<Envelope>,
}

impl<Tx: TxManager> SubmissionTask<Tx> {
    /// Builds the task and its paired [`SubmissionHandle`].
    /// `capacity` bounds the underlying mpsc channel.
    pub fn new(tx_manager: Tx, capacity: usize) -> (Self, SubmissionHandle) {
        let (sender, receiver) = mpsc::channel(capacity);
        (Self { tx_manager, receiver }, SubmissionHandle { sender })
    }

    /// Drains envelopes until every [`SubmissionHandle`] is dropped or
    /// `cancel` fires.
    pub async fn run(mut self, cancel: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                envelope = self.receiver.recv() => match envelope {
                    Some(e) => self.handle(e).await,
                    None => return,
                },
            }
        }
    }

    /// Submits one envelope and forwards the outcome to its caller.
    async fn handle(&self, envelope: Envelope) {
        let game = envelope.submission.game_address();
        let candidate = TxCandidate {
            tx_data: envelope.submission.into_calldata(),
            to: Some(game),
            ..TxCandidate::default()
        };

        let outcome = match self.tx_manager.send(candidate).await {
            Ok(receipt) if receipt.status() => Ok(receipt.transaction_hash),
            Ok(receipt) => Err(SubmitError::Reverted(receipt.transaction_hash)),
            Err(e) => Err(SubmitError::TxManager(e)),
        };

        if envelope.response_tx.send(outcome).is_err() {
            debug!(%game, "submission caller dropped before outcome");
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, address, b256};
    use base_tx_manager::TxManagerError;

    use super::*;
    use crate::{BondAction, DisputeAction, test_utils::MockTxManager};

    const GAME: Address = address!("00000000000000000000000000000000000000a1");
    const SENDER: Address = address!("00000000000000000000000000000000000000b2");
    const TX_HASH: B256 = b256!("9999999999999999999999999999999999999999999999999999999999999999");

    fn challenge(index: u64, our_root: B256) -> DisputeAction {
        DisputeAction::Challenge {
            index,
            our_root,
            starting_root: B256::ZERO,
            start_block: 100,
            end_block: 200,
        }
    }

    fn nullify_tee(index: u64, our_root: B256) -> DisputeAction {
        DisputeAction::NullifyTee {
            index,
            our_root,
            starting_root: B256::ZERO,
            start_block: 100,
            end_block: 200,
        }
    }

    fn nullify_zk(index: u64, root_to_prove: B256) -> DisputeAction {
        DisputeAction::NullifyZk {
            index,
            root_to_prove,
            starting_root: B256::ZERO,
            start_block: 100,
            end_block: 200,
        }
    }

    fn dispute(action: DisputeAction, proof_first_byte: u8) -> Submission {
        Submission::Dispute(DisputeRequest {
            game_address: GAME,
            action,
            proof_bytes: Bytes::from(vec![proof_first_byte, 0x42]),
        })
    }

    fn bond(action: BondAction) -> Submission {
        Submission::Bond(BondRequest { game_address: GAME, action })
    }

    /// Spawns a [`SubmissionTask`] backed by `tx_manager` and returns
    /// `(handle, cancel, join_handle)`. Cancelling and joining is the
    /// caller's responsibility.
    fn spawn(
        tx_manager: MockTxManager,
    ) -> (SubmissionHandle, CancellationToken, tokio::task::JoinHandle<()>) {
        let (task, handle) = SubmissionTask::new(tx_manager, 8);
        let cancel = CancellationToken::new();
        let join = tokio::spawn(task.run(cancel.clone()));
        (handle, cancel, join)
    }

    #[tokio::test]
    async fn submit_returns_tx_hash_on_success() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        let (handle, cancel, join) = spawn(tx_manager.clone());

        let outcome = handle.submit(dispute(challenge(2, B256::repeat_byte(0x11)), 0x01)).await;

        assert_eq!(outcome.unwrap(), TX_HASH);
        let calls = tx_manager.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, Some(GAME));

        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test]
    async fn submit_returns_reverted_when_status_is_false() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_revert(TX_HASH);
        let (handle, cancel, join) = spawn(tx_manager);

        let outcome = handle.submit(dispute(nullify_tee(2, B256::repeat_byte(0x22)), 0x00)).await;

        assert!(matches!(outcome, Err(SubmitError::Reverted(h)) if h == TX_HASH));

        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test]
    async fn submit_returns_tx_manager_when_send_errors() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_error(TxManagerError::NonceTooLow);
        let (handle, cancel, join) = spawn(tx_manager);

        let outcome = handle.submit(dispute(nullify_zk(2, B256::repeat_byte(0x33)), 0x01)).await;

        assert!(matches!(outcome, Err(SubmitError::TxManager(TxManagerError::NonceTooLow))));

        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test]
    async fn submit_returns_channel_closed_after_task_shutdown() {
        let tx_manager = MockTxManager::new(SENDER);
        let (handle, cancel, join) = spawn(tx_manager);
        cancel.cancel();
        join.await.unwrap();

        let outcome = handle.submit(bond(BondAction::Resolve)).await;

        assert!(matches!(outcome, Err(SubmitError::ChannelClosed)));
    }

    #[tokio::test]
    async fn bond_submission_carries_bond_calldata() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        let (handle, cancel, join) = spawn(tx_manager.clone());

        handle.submit(bond(BondAction::UnlockCredit)).await.unwrap();

        let calls = tx_manager.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, Some(GAME));
        assert_eq!(calls[0].tx_data, BondAction::UnlockCredit.to_calldata());

        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test]
    async fn dispute_tx_data_matches_into_calldata() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        let (handle, cancel, join) = spawn(tx_manager.clone());
        let submission = dispute(challenge(2, B256::repeat_byte(0x11)), 0x01);
        let expected = submission.clone().into_calldata();

        handle.submit(submission).await.unwrap();

        let call = tx_manager.calls().pop().expect("one tx submitted");
        assert_eq!(call.tx_data, expected);
        assert!(call.blobs.is_empty(), "challenger never sends blob txs");

        cancel.cancel();
        join.await.unwrap();
    }

    #[tokio::test]
    async fn cancel_token_exits_immediately() {
        let (task, _handle) = SubmissionTask::new(MockTxManager::new(SENDER), 8);
        let cancel = CancellationToken::new();
        let join = tokio::spawn(task.run(cancel.clone()));

        cancel.cancel();
        join.await.expect("run must exit cleanly on cancel");
    }

    #[tokio::test]
    async fn dropping_every_handle_exits_run() {
        let (task, handle) = SubmissionTask::new(MockTxManager::new(SENDER), 8);
        let join = tokio::spawn(task.run(CancellationToken::new()));

        drop(handle);
        join.await.expect("run must exit when handles drop");
    }

    #[tokio::test]
    async fn processes_pending_submission_before_exit() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        let (task, handle) = SubmissionTask::new(tx_manager.clone(), 8);
        let join = tokio::spawn(task.run(CancellationToken::new()));

        let outcome = handle.submit(dispute(challenge(2, B256::repeat_byte(0x11)), 0x01)).await;
        assert_eq!(outcome.unwrap(), TX_HASH);

        drop(handle);
        join.await.expect("run must exit cleanly after draining");
        assert_eq!(tx_manager.calls().len(), 1);
    }

    #[tokio::test]
    async fn submission_display_delegates_to_inner_action() {
        assert_eq!(dispute(challenge(2, B256::ZERO), 0x01).to_string(), "Challenge");
        assert_eq!(bond(BondAction::Resolve).to_string(), "Resolve");
    }
}
