//! Submission task: drains [`Submission`]s from the game and bond
//! workers and submits each one through a single [`TxManager`] for
//! nonce safety.
//!
//! The contract itself rejects stale or invalid submissions at
//! `eth_estimateGas` time, so no client-side re-verification is needed.

use std::fmt;

use alloy_primitives::{Address, Bytes};
use base_tx_manager::{TxCandidate, TxManager};
use derive_more::Debug;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{BondRequest, DisputeRequest};

/// One unit of L1 work routed through [`SubmissionTask`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submission {
    /// Dispute action (challenge or nullify) produced by a game worker.
    Dispute(DisputeRequest),
    /// Bond lifecycle action (resolve, claimCredit, closeGame)
    /// produced by a bond worker.
    Bond(BondRequest),
}

impl Submission {
    /// Game proxy this submission targets.
    pub const fn game_address(&self) -> Address {
        match self {
            Self::Dispute(r) => r.game_address,
            Self::Bond(r) => r.game_address,
        }
    }

    /// Encodes the L1 calldata for this submission.
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

/// Long-running task that drains [`Submission`]s, encodes the
/// calldata, and submits the transaction through a single
/// [`TxManager`].
///
/// Routing every submission through one task is a deliberate choice:
/// the underlying [`TxManager`] then sees a serial stream of
/// transactions from a single sender, which is the only way to
/// avoid nonce races without coordinating across workers.
#[derive(Debug)]
pub struct SubmissionTask<Tx> {
    /// Sender used for every submission; serial use here is what
    /// guarantees nonce safety.
    #[debug(skip)]
    tx_manager: Tx,
}

impl<Tx: TxManager> SubmissionTask<Tx> {
    /// Builds a task wired to `tx_manager` for L1 submission.
    pub const fn new(tx_manager: Tx) -> Self {
        Self { tx_manager }
    }

    /// Drains `rx` until it closes or `cancel` fires, handling each
    /// submission to completion before reading the next. Sequential
    /// processing keeps the [`TxManager`]'s nonce stream contiguous
    /// and bounds in-flight L1 work to one transaction at a time.
    pub async fn run(self, mut rx: mpsc::Receiver<Submission>, cancel: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                submission = rx.recv() => match submission {
                    Some(s) => self.handle(s).await,
                    None => return,
                },
            }
        }
    }

    /// Encodes the submission's calldata and submits the transaction.
    async fn handle(&self, submission: Submission) {
        let game = submission.game_address();
        let label = submission.to_string();
        let candidate = TxCandidate {
            tx_data: submission.into_calldata(),
            to: Some(game),
            ..TxCandidate::default()
        };

        match self.tx_manager.send(candidate).await {
            Ok(receipt) if receipt.status() => info!(
                %game,
                action = %label,
                tx_hash = %receipt.transaction_hash,
                "submission confirmed on L1"
            ),
            Ok(receipt) => warn!(
                %game,
                action = %label,
                tx_hash = %receipt.transaction_hash,
                "submission reverted at inclusion"
            ),
            Err(e) => warn!(
                %game,
                action = %label,
                error = %e,
                "submission failed before confirmation"
            ),
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

    fn task(tx_manager: MockTxManager) -> SubmissionTask<MockTxManager> {
        SubmissionTask::new(tx_manager)
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

    #[tokio::test]
    async fn challenge_success_submits_one_tx_to_game_address() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);

        task(tx_manager.clone()).handle(dispute(challenge(2, B256::repeat_byte(0x11)), 0x01)).await;

        let calls = tx_manager.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, Some(GAME));
    }

    #[tokio::test]
    async fn nullify_tee_success_submits_one_tx() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);

        task(tx_manager.clone())
            .handle(dispute(nullify_tee(2, B256::repeat_byte(0x22)), 0x00))
            .await;

        assert_eq!(tx_manager.calls().len(), 1);
    }

    #[tokio::test]
    async fn nullify_zk_success_submits_one_tx() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);

        task(tx_manager.clone())
            .handle(dispute(nullify_zk(2, B256::repeat_byte(0x33)), 0x01))
            .await;

        assert_eq!(tx_manager.calls().len(), 1);
    }

    #[tokio::test]
    async fn bond_resolve_submits_one_tx_to_game_address() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);

        task(tx_manager.clone()).handle(bond(BondAction::Resolve)).await;

        let calls = tx_manager.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, Some(GAME));
        assert_eq!(calls[0].tx_data, BondAction::Resolve.to_calldata());
    }

    #[tokio::test]
    async fn bond_unlock_credit_submits_claim_credit_calldata() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);

        task(tx_manager.clone()).handle(bond(BondAction::UnlockCredit)).await;

        let calls = tx_manager.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tx_data, BondAction::UnlockCredit.to_calldata());
    }

    #[tokio::test]
    async fn dispute_tx_data_matches_action_to_calldata() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        let submission = dispute(challenge(2, B256::repeat_byte(0x11)), 0x01);
        let expected = submission.clone().into_calldata();

        task(tx_manager.clone()).handle(submission).await;

        let call = tx_manager.calls().pop().expect("one tx submitted");
        assert_eq!(call.tx_data, expected);
        assert!(call.blobs.is_empty(), "challenger never sends blob txs");
    }

    #[tokio::test]
    async fn revert_at_inclusion_does_not_panic() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_revert(TX_HASH);

        task(tx_manager.clone()).handle(dispute(challenge(2, B256::repeat_byte(0x11)), 0x01)).await;

        assert_eq!(tx_manager.calls().len(), 1);
    }

    #[tokio::test]
    async fn tx_manager_error_does_not_panic() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_error(TxManagerError::NonceTooLow);

        task(tx_manager.clone()).handle(dispute(challenge(2, B256::repeat_byte(0x11)), 0x01)).await;

        assert_eq!(tx_manager.calls().len(), 1);
    }

    #[tokio::test]
    async fn cancel_token_exits_immediately() {
        let task = SubmissionTask::new(MockTxManager::new(SENDER));
        let (_tx, rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(task.run(rx, cancel.clone()));
        cancel.cancel();
        handle.await.expect("run must exit cleanly");
    }

    #[tokio::test]
    async fn closed_request_channel_exits_cleanly() {
        let task = SubmissionTask::new(MockTxManager::new(SENDER));
        let (tx, rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(task.run(rx, cancel));
        drop(tx);
        handle.await.expect("run must exit when senders drop");
    }

    #[tokio::test]
    async fn processes_pending_submission_before_exit() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        let task = SubmissionTask::new(tx_manager.clone());
        let (tx, rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(task.run(rx, cancel));
        tx.send(dispute(challenge(2, B256::repeat_byte(0x11)), 0x01))
            .await
            .expect("send must succeed");
        drop(tx);
        handle.await.expect("run must exit cleanly after draining");

        assert_eq!(tx_manager.calls().len(), 1);
    }

    #[tokio::test]
    async fn submission_display_delegates_to_inner_action() {
        assert_eq!(dispute(challenge(2, B256::ZERO), 0x01).to_string(), "Challenge");
        assert_eq!(bond(BondAction::Resolve).to_string(), "Resolve");
    }
}
