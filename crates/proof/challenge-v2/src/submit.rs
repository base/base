//! Submission pipeline: encodes [`DisputeAction`]s and submits them
//! through a single [`TxManager`] for nonce safety.
//!
//! Game workers produce [`DisputeRequest`]s via
//! [`crate::Violation::dispute_request`]; the [`SubmissionTask`]
//! drains those requests, encodes the matching calldata, and sends
//! them. The contract itself rejects stale or invalid submissions
//! at `eth_estimateGas` time, so no client-side re-verification is
//! needed.

use std::fmt;

use alloy_primitives::{Address, B256, Bytes};
use base_proof_contracts::{encode_challenge_calldata, encode_nullify_calldata};
use base_tx_manager::{TxCandidate, TxManager};
use derive_more::Debug;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::ChallengerMetrics;

/// On-chain action this challenger will submit against a dispute game.
///
/// Carries the call parameters for the matching `AggregateVerifier`
/// entrypoint. The proof bytes (TEE signature, ZK SNARK) are produced
/// separately by [`crate::Violation::dispute_request`] and bundled
/// into [`DisputeRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisputeAction {
    /// Calls `challenge(index, our_root)`. Used as failover when
    /// [`Self::NullifyTee`] cannot be produced; see
    /// [`crate::Violation::dispute_request`] for the exact fallback
    /// conditions.
    Challenge {
        /// Intermediate root index disputed by the challenger.
        index: u64,
        /// The root we are asserting (computed from our L2 RPC).
        our_root: B256,
        /// Predecessor root (anchor or previous intermediate root).
        starting_root: B256,
        /// L2 block at the start of the challenged range.
        start_block: u64,
        /// L2 block at the end of the challenged range.
        end_block: u64,
    },
    /// Calls `nullify(index, our_root)` with TEE proof bytes
    /// (`proof_type == 0`). Kills `TEE_VERIFIER` globally.
    NullifyTee {
        /// Intermediate root index disputed by the challenger.
        index: u64,
        /// Root attested by our TEE prover.
        our_root: B256,
        /// Predecessor root (anchor or previous intermediate root).
        starting_root: B256,
        /// L2 block at the start of the challenged range.
        start_block: u64,
        /// L2 block at the end of the challenged range.
        end_block: u64,
    },
    /// Calls `nullify(index, root_to_prove)` with ZK proof bytes
    /// (`proof_type == 1`). Kills `ZK_VERIFIER` globally.
    NullifyZk {
        /// Intermediate root index disputed by the challenger.
        index: u64,
        /// Root the SNARK is asserting (our computed L2 root for
        /// `ZkWrong`, on-chain TEE root for `FraudulentZkChallenge`).
        root_to_prove: B256,
        /// Predecessor root (anchor or previous intermediate root).
        starting_root: B256,
        /// L2 block at the start of the challenged range.
        start_block: u64,
        /// L2 block at the end of the challenged range.
        end_block: u64,
    },
}

impl DisputeAction {
    /// Returns the intermediate root index this action targets.
    pub const fn index(&self) -> u64 {
        match self {
            Self::Challenge { index, .. }
            | Self::NullifyTee { index, .. }
            | Self::NullifyZk { index, .. } => *index,
        }
    }

    /// Encodes the L1 calldata for this action, prepending the proof
    /// type discriminator already baked into `proof_bytes`.
    ///
    /// Dispatches to the matching `AggregateVerifier` entrypoint:
    /// - [`Self::Challenge`] uses [`encode_challenge_calldata`].
    /// - [`Self::NullifyTee`] and [`Self::NullifyZk`] use
    ///   [`encode_nullify_calldata`]; the encoder picks the matching
    ///   verifier by reading the first byte of `proof_bytes`
    ///   (`0` for TEE, `1` for ZK).
    pub fn to_calldata(&self, proof_bytes: Bytes) -> Bytes {
        match self {
            Self::Challenge { index, our_root, .. } => {
                encode_challenge_calldata(proof_bytes, *index, *our_root)
            }
            Self::NullifyTee { index, our_root, .. } => {
                encode_nullify_calldata(proof_bytes, *index, *our_root)
            }
            Self::NullifyZk { index, root_to_prove, .. } => {
                encode_nullify_calldata(proof_bytes, *index, *root_to_prove)
            }
        }
    }

    /// Snake-case label used as the `action` value on challenger
    /// metrics. Distinct from [`fmt::Display`], which is the
    /// human-readable variant name used in tracing fields.
    const fn metric_label(&self) -> &'static str {
        match self {
            Self::Challenge { .. } => ChallengerMetrics::ACTION_CHALLENGE,
            Self::NullifyTee { .. } => ChallengerMetrics::ACTION_NULLIFY_TEE,
            Self::NullifyZk { .. } => ChallengerMetrics::ACTION_NULLIFY_ZK,
        }
    }
}

impl fmt::Display for DisputeAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Challenge { .. } => "Challenge",
            Self::NullifyTee { .. } => "NullifyTee",
            Self::NullifyZk { .. } => "NullifyZk",
        })
    }
}

/// A `DisputeAction` bundled with the proof bytes that prove it.
///
/// Produced by [`crate::Violation::dispute_request`] and consumed by
/// [`SubmissionTask`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisputeRequest {
    /// Dispute game proxy this action targets.
    pub game_address: Address,
    /// Action to call on the game contract.
    pub action: DisputeAction,
    /// Proof bytes prefixed with the proof type discriminator
    /// (`PROOF_TYPE_TEE = 0` or `PROOF_TYPE_ZK = 1`).
    pub proof_bytes: Bytes,
}

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
        let action_label = request.action.metric_label();

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

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};
    use base_tx_manager::TxManagerError;

    use super::*;

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

    mod to_calldata {
        use super::*;

        #[test]
        fn challenge_dispatches_to_encode_challenge_calldata() {
            let proof = Bytes::from(vec![0x01, 0xAA, 0xBB]);
            let root = B256::repeat_byte(0xAB);

            let calldata = challenge(7, root).to_calldata(proof.clone());

            assert_eq!(calldata, encode_challenge_calldata(proof, 7, root));
        }

        #[test]
        fn nullify_tee_dispatches_to_encode_nullify_with_our_root() {
            let proof = Bytes::from(vec![0x00, 0xCC, 0xDD]);
            let root = B256::repeat_byte(0xCD);

            let calldata = nullify_tee(3, root).to_calldata(proof.clone());

            assert_eq!(calldata, encode_nullify_calldata(proof, 3, root));
        }

        #[test]
        fn nullify_zk_dispatches_to_encode_nullify_with_root_to_prove() {
            let proof = Bytes::from(vec![0x01, 0xEE, 0xFF]);
            let root = B256::repeat_byte(0xEF);

            let calldata = nullify_zk(5, root).to_calldata(proof.clone());

            assert_eq!(calldata, encode_nullify_calldata(proof, 5, root));
        }

        #[test]
        fn ignores_starting_root_and_block_range_fields() {
            // starting_root, start_block and end_block are local context for
            // submission ergonomics; they must not appear in the L1 calldata.
            let proof = Bytes::from(vec![0x01]);
            let root = B256::repeat_byte(0x42);
            let same_index_same_root = DisputeAction::Challenge {
                index: 1,
                our_root: root,
                starting_root: B256::repeat_byte(0xAA),
                start_block: 1,
                end_block: 2,
            };
            let other_context = DisputeAction::Challenge {
                index: 1,
                our_root: root,
                starting_root: B256::repeat_byte(0xBB),
                start_block: 999,
                end_block: 1000,
            };

            assert_eq!(
                same_index_same_root.to_calldata(proof.clone()),
                other_context.to_calldata(proof),
            );
        }
    }

    mod submission_task {
        use super::*;
        use crate::test_utils::MockTxManager;

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

            task(tx_manager.clone())
                .handle(request(challenge(2, B256::repeat_byte(0x11)), 0x01))
                .await;

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

            task(tx_manager.clone())
                .handle(request(challenge(2, B256::repeat_byte(0x11)), 0x01))
                .await;

            assert_eq!(tx_manager.calls().len(), 1);
        }

        #[tokio::test]
        async fn tx_manager_error_does_not_panic() {
            let tx_manager = MockTxManager::new(SENDER);
            tx_manager.push_error(TxManagerError::NonceTooLow);

            task(tx_manager.clone())
                .handle(request(challenge(2, B256::repeat_byte(0x11)), 0x01))
                .await;

            assert_eq!(tx_manager.calls().len(), 1);
        }
    }

    mod run {
        use super::*;
        use crate::test_utils::MockTxManager;

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
}
