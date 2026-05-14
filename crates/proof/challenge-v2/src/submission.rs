//! Submission task: drains [`DisputeRequest`]s from the game workers
//! and submits each one through a single [`TxManager`] for nonce safety.
//!
//! The contract itself rejects stale or invalid submissions at
//! `eth_estimateGas` time, so no client-side re-verification is needed.

use base_tx_manager::{TxCandidate, TxManager};
use derive_more::Debug;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{ChallengerMetrics, DisputeAction, DisputeRequest};

/// Long-running task that drains [`DisputeRequest`]s, encodes the
/// calldata, and submits the transaction through a single
/// [`TxManager`].
///
/// Routing every dispute through one task is a deliberate choice:
/// the underlying [`TxManager`] then sees a serial stream of
/// transactions from a single sender, which is the only way to
/// avoid nonce races without coordinating across workers.
#[derive(Debug)]
pub struct SubmissionTask<Tx> {
    /// Sender used for every dispute transaction; serial use here
    /// is what guarantees nonce safety.
    #[debug(skip)]
    tx_manager: Tx,
}

impl<Tx: TxManager> SubmissionTask<Tx> {
    /// Builds a task wired to `tx_manager` for L1 submission.
    pub const fn new(tx_manager: Tx) -> Self {
        Self { tx_manager }
    }

    /// Drains `rx` until it closes or `cancel` fires, handling each
    /// request to completion before reading the next. Sequential
    /// processing keeps the [`TxManager`]'s nonce stream contiguous
    /// and bounds in-flight L1 work to one transaction at a time.
    pub async fn run(self, mut rx: mpsc::Receiver<DisputeRequest>, cancel: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                request = rx.recv() => match request {
                    Some(req) => self.handle(req).await,
                    None => return,
                },
            }
        }
    }

    /// Encodes the request's calldata, submits it, and records the
    /// outcome via metrics.
    async fn handle(&self, request: DisputeRequest) {
        let game = request.game_address;
        let action_label = action_metric_label(&request.action);

        let candidate = TxCandidate {
            tx_data: request.action.to_calldata(request.proof_bytes),
            to: Some(game),
            ..TxCandidate::default()
        };

        let started = std::time::Instant::now();
        let outcome = self.tx_manager.send(candidate).await;
        ChallengerMetrics::submit_duration_seconds(action_label)
            .record(started.elapsed().as_secs_f64());

        let status_label = match outcome {
            Ok(receipt) if receipt.status() => {
                info!(
                    %game,
                    action = %request.action,
                    tx_hash = %receipt.transaction_hash,
                    "submission confirmed on L1"
                );
                ChallengerMetrics::SUBMIT_STATUS_SUCCESS
            }
            Ok(receipt) => {
                // Tx mined but EVM reverted; see receipt and log
                // for the cause.
                warn!(
                    %game,
                    action = %request.action,
                    tx_hash = %receipt.transaction_hash,
                    "submission reverted at inclusion"
                );
                ChallengerMetrics::SUBMIT_STATUS_REVERTED
            }
            Err(e) => {
                // TxManager failed somewhere in the send pipeline.
                warn!(
                    %game,
                    action = %request.action,
                    error = %e,
                    "submission failed before confirmation"
                );
                ChallengerMetrics::SUBMIT_STATUS_ERROR
            }
        };
        ChallengerMetrics::submit_outcome_total(action_label, status_label).increment(1);
    }
}

/// Snake-case label used as the `action` value on challenger metrics.
/// Distinct from `Display`, which renders the human-readable variant
/// name used in tracing fields.
const fn action_metric_label(action: &DisputeAction) -> &'static str {
    match action {
        DisputeAction::Challenge { .. } => ChallengerMetrics::ACTION_CHALLENGE,
        DisputeAction::NullifyTee { .. } => ChallengerMetrics::ACTION_NULLIFY_TEE,
        DisputeAction::NullifyZk { .. } => ChallengerMetrics::ACTION_NULLIFY_ZK,
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, address, b256};
    use base_tx_manager::TxManagerError;

    use super::*;
    use crate::test_utils::MockTxManager;

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

    fn request(action: DisputeAction, proof_first_byte: u8) -> DisputeRequest {
        DisputeRequest {
            game_address: GAME,
            action,
            proof_bytes: Bytes::from(vec![proof_first_byte, 0x42]),
        }
    }

    #[tokio::test]
    async fn challenge_success_submits_one_tx_to_game_address() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);

        task(tx_manager.clone()).handle(request(challenge(2, B256::repeat_byte(0x11)), 0x01)).await;

        let calls = tx_manager.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, Some(GAME));
    }

    #[tokio::test]
    async fn nullify_tee_success_submits_one_tx() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);

        task(tx_manager.clone())
            .handle(request(nullify_tee(2, B256::repeat_byte(0x22)), 0x00))
            .await;

        assert_eq!(tx_manager.calls().len(), 1);
    }

    #[tokio::test]
    async fn nullify_zk_success_submits_one_tx() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);

        task(tx_manager.clone())
            .handle(request(nullify_zk(2, B256::repeat_byte(0x33)), 0x01))
            .await;

        assert_eq!(tx_manager.calls().len(), 1);
    }

    #[tokio::test]
    async fn tx_data_matches_action_to_calldata() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        let req = request(challenge(2, B256::repeat_byte(0x11)), 0x01);
        let expected = req.action.to_calldata(req.proof_bytes.clone());

        task(tx_manager.clone()).handle(req).await;

        let call = tx_manager.calls().pop().expect("one tx submitted");
        assert_eq!(call.tx_data, expected);
        assert!(call.blobs.is_empty(), "challenger never sends blob txs");
    }

    #[tokio::test]
    async fn revert_at_inclusion_does_not_panic() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_revert(TX_HASH);

        task(tx_manager.clone()).handle(request(challenge(2, B256::repeat_byte(0x11)), 0x01)).await;

        assert_eq!(tx_manager.calls().len(), 1);
    }

    #[tokio::test]
    async fn tx_manager_error_does_not_panic() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_error(TxManagerError::NonceTooLow);

        task(tx_manager.clone()).handle(request(challenge(2, B256::repeat_byte(0x11)), 0x01)).await;

        assert_eq!(tx_manager.calls().len(), 1);
    }

    #[tokio::test]
    async fn cancel_token_exits_immediately() {
        let task = SubmissionTask::new(MockTxManager::new(SENDER));
        let (_request_tx, request_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(task.run(request_rx, cancel.clone()));
        cancel.cancel();
        handle.await.expect("run must exit cleanly");
    }

    #[tokio::test]
    async fn closed_request_channel_exits_cleanly() {
        let task = SubmissionTask::new(MockTxManager::new(SENDER));
        let (request_tx, request_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(task.run(request_rx, cancel));
        drop(request_tx);
        handle.await.expect("run must exit when senders drop");
    }

    #[tokio::test]
    async fn processes_pending_request_before_exit() {
        let tx_manager = MockTxManager::new(SENDER);
        tx_manager.push_success(TX_HASH);
        let task = SubmissionTask::new(tx_manager.clone());
        let (request_tx, request_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let handle = tokio::spawn(task.run(request_rx, cancel));
        request_tx
            .send(DisputeRequest {
                game_address: GAME,
                action: challenge(2, B256::repeat_byte(0x11)),
                proof_bytes: Bytes::from(vec![0x01]),
            })
            .await
            .expect("send must succeed");
        drop(request_tx);
        handle.await.expect("run must exit cleanly after draining");

        assert_eq!(tx_manager.calls().len(), 1);
    }
}
