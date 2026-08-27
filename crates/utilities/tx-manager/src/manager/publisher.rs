//! Symmetric, nonce-ordered transaction publication across RPC backends.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    panic::AssertUnwindSafe,
    sync::Arc,
    time::Duration,
};

use alloy_primitives::B256;
use alloy_provider::Provider;
use base_runtime::{Runtime, RuntimeTimeout};
use futures::FutureExt;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use super::{
    build::PreparedTx,
    pending::{RejectionVerdict, VersionId, VersionKind},
};
use crate::{SubmissionId, TxManagerError, TxMetrics, error::RpcErrorClassifier};

/// Stable index of one publication backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublisherId(
    /// Zero-based position in the configured backend list.
    usize,
);

impl PublisherId {
    /// Creates a publisher identifier from its zero-based index.
    pub const fn new(index: usize) -> Self {
        Self(index)
    }

    /// Returns the zero-based backend index.
    pub const fn index(self) -> usize {
        self.0
    }
}

/// Result of one bounded publication attempt.
#[derive(Debug, Clone)]
pub enum PublishOutcome {
    /// The backend positively acknowledged these signed bytes.
    Accepted,
    /// The request may have reached the backend, but no definitive answer was observed.
    Ambiguous,
    /// The backend definitively rejected the transaction.
    Rejected(PublishReject),
}

impl PublishOutcome {
    /// Classifies an RPC failure by whether signed bytes may have reached the backend.
    pub fn classify_error(error: TxManagerError) -> Self {
        match error {
            TxManagerError::AlreadyKnown => Self::Accepted,
            TxManagerError::Transport(_) | TxManagerError::Rpc(_) => Self::Ambiguous,
            TxManagerError::NonceTooHigh => Self::Rejected(PublishReject::NonceTooHigh),
            TxManagerError::NonceTooLow => Self::Rejected(PublishReject::NonceTooLow),
            TxManagerError::AlreadyReserved => Self::Rejected(PublishReject::AlreadyReserved),
            error @ TxManagerError::TxPoolFull => Self::Rejected(PublishReject::Transient(error)),
            error @ (TxManagerError::Underpriced
            | TxManagerError::ReplacementUnderpriced
            | TxManagerError::FeeTooLow
            | TxManagerError::MaxFeePerGasTooLow) => {
                Self::Rejected(PublishReject::FeeTooLow(error))
            }
            error => Self::Rejected(PublishReject::Deterministic(error)),
        }
    }
}

/// Definitive rejection classes consumed by the pending ledger.
#[derive(Debug, Clone)]
pub enum PublishReject {
    /// The nonce is above the backend's next executable nonce.
    NonceTooHigh,
    /// The nonce was already consumed.
    NonceTooLow,
    /// The transaction or replacement requires higher fees.
    FeeTooLow(TxManagerError),
    /// The account is reserved by an incompatible txpool transaction.
    AlreadyReserved,
    /// The backend definitively rejected the transaction for a temporary reason.
    Transient(TxManagerError),
    /// A deterministic publication failure.
    Deterministic(TxManagerError),
}

impl PublishReject {
    /// Returns the public error represented by this rejection.
    pub fn as_error(&self) -> TxManagerError {
        match self {
            Self::NonceTooHigh => TxManagerError::NonceTooHigh,
            Self::NonceTooLow => TxManagerError::NonceTooLow,
            Self::FeeTooLow(error) | Self::Transient(error) | Self::Deterministic(error) => {
                error.clone()
            }
            Self::AlreadyReserved => TxManagerError::AlreadyReserved,
        }
    }

    /// Reduces one fully rejected publication pass to a single state-machine decision.
    pub fn verdict(rejections: &[Self]) -> RejectionVerdict {
        if rejections.iter().any(|rejection| matches!(rejection, Self::NonceTooLow)) {
            return RejectionVerdict::NonceTooLow;
        }

        if rejections.iter().all(|rejection| matches!(rejection, Self::Deterministic(_))) {
            let error = rejections.first().map_or(TxManagerError::ChannelClosed, Self::as_error);
            return RejectionVerdict::Deterministic(error);
        }

        let has_deterministic =
            rejections.iter().any(|rejection| matches!(rejection, Self::Deterministic(_)));
        if !has_deterministic
            && let Some(rejection) =
                rejections.iter().find(|rejection| matches!(rejection, Self::FeeTooLow(_)))
        {
            return RejectionVerdict::FeeTooLow(rejection.as_error());
        }

        RejectionVerdict::Retry(
            rejections.first().map_or(TxManagerError::ChannelClosed, Self::as_error),
        )
    }
}

/// One current signed ledger entry visible to every publisher.
#[derive(Debug, Clone)]
pub struct PublisherTx {
    /// Logical submission that owns the nonce.
    pub submission_id: SubmissionId,
    /// Account nonce encoded in the signed transaction.
    pub nonce: u64,
    /// Current signed version within the nonce slot.
    pub version: VersionId,
    /// Coordinator-controlled attempt epoch for this version.
    pub epoch: u64,
    /// Semantic purpose of the signed version.
    pub kind: VersionKind,
    /// Immutable signed bytes shared by every backend.
    pub prepared: PreparedTx,
}

/// Latest immutable view of the signed pending ledger.
#[derive(Debug, Clone)]
pub struct PublisherSnapshot {
    /// Monotonic ledger revision used to coalesce duplicate notifications.
    pub revision: u64,
    /// Current signed entries in nonce order.
    pub transactions: Arc<[PublisherTx]>,
}

impl PublisherSnapshot {
    /// Creates the empty snapshot used before the coordinator publishes state.
    pub fn empty() -> Self {
        Self { revision: 0, transactions: Arc::from([]) }
    }
}

/// Result returned by one publisher worker to the coordinator.
#[derive(Debug, Clone)]
pub struct PublisherEvent {
    /// Backend that attempted publication.
    pub publisher: PublisherId,
    /// Logical submission that owns the transaction.
    pub submission_id: SubmissionId,
    /// Signed version that was attempted.
    pub version: VersionId,
    /// Attempt epoch observed by the worker.
    pub epoch: u64,
    /// Semantic purpose of the attempted version.
    pub kind: VersionKind,
    /// Canonical hash of the signed transaction.
    pub tx_hash: B256,
    /// Classified backend response.
    pub outcome: PublishOutcome,
}

/// Performs bounded publication against one backend.
#[derive(Debug, Clone)]
pub struct TxPublisher<P, R> {
    /// Stable backend index used for operational context.
    id: PublisherId,
    /// RPC provider used only for transaction publication.
    provider: P,
    /// Runtime used to enforce request deadlines.
    runtime: R,
    /// Maximum duration of one publication request.
    network_timeout: Duration,
    /// Metrics sink for publication and RPC failures.
    metrics: Arc<dyn TxMetrics>,
}

impl<P, R> TxPublisher<P, R>
where
    P: Provider + Clone + Debug + Send + Sync + 'static,
    R: Runtime,
{
    /// Creates a publisher for one backend.
    pub fn new(
        id: PublisherId,
        provider: P,
        runtime: R,
        network_timeout: Duration,
        metrics: Arc<dyn TxMetrics>,
    ) -> Self {
        Self { id, provider, runtime, network_timeout, metrics }
    }

    /// Performs one bounded publication attempt.
    pub async fn publish(&self, prepared: &PreparedTx) -> PublishOutcome {
        let result = RuntimeTimeout::run(
            &self.runtime,
            self.network_timeout,
            self.provider.send_raw_transaction(&prepared.raw_tx),
        )
        .await;

        match result {
            Ok(Ok(pending)) => {
                // A successful response is valid only when it names the signed
                // envelope that every backend received.
                let returned_hash = *pending.tx_hash();
                if returned_hash != prepared.tx_hash {
                    self.metrics.record_rpc_error();
                    self.metrics.record_publish_error();
                    warn!(
                        backend = self.id.index(),
                        expected_hash = %prepared.tx_hash,
                        returned_hash = %returned_hash,
                        "publication backend returned a mismatched transaction hash",
                    );
                    return PublishOutcome::Ambiguous;
                }
                info!(
                    backend = self.id.index(),
                    tx_hash = %prepared.tx_hash,
                    nonce = prepared.nonce,
                    "transaction published",
                );
                PublishOutcome::Accepted
            }
            Ok(Err(error)) => {
                let classified = RpcErrorClassifier::classify_rpc_error(&error);
                if classified.is_rpc_error() {
                    self.metrics.record_rpc_error();
                    self.metrics.record_publish_error();
                }
                let outcome = PublishOutcome::classify_error(classified.clone());
                match &outcome {
                    PublishOutcome::Accepted => {
                        info!(
                            backend = self.id.index(),
                            tx_hash = %prepared.tx_hash,
                            nonce = prepared.nonce,
                            "transaction already known by publication backend",
                        );
                    }
                    PublishOutcome::Ambiguous => {
                        warn!(
                            backend = self.id.index(),
                            error_kind = classified.kind(),
                            tx_hash = %prepared.tx_hash,
                            nonce = prepared.nonce,
                            "transaction publication outcome is ambiguous",
                        );
                    }
                    PublishOutcome::Rejected(PublishReject::Deterministic(_)) => {
                        warn!(
                            backend = self.id.index(),
                            error_kind = classified.kind(),
                            tx_hash = %prepared.tx_hash,
                            nonce = prepared.nonce,
                            "transaction publication rejected",
                        );
                    }
                    PublishOutcome::Rejected(_) => {
                        info!(
                            backend = self.id.index(),
                            error_kind = classified.kind(),
                            tx_hash = %prepared.tx_hash,
                            nonce = prepared.nonce,
                            "transaction publication rejected",
                        );
                    }
                }
                outcome
            }
            Err(_) => {
                // A timeout cannot prove rejection: the backend may have
                // accepted the bytes before the response path failed.
                self.metrics.record_rpc_error();
                self.metrics.record_publish_error();
                warn!(
                    backend = self.id.index(),
                    tx_hash = %prepared.tx_hash,
                    nonce = prepared.nonce,
                    timeout = ?self.network_timeout,
                    "transaction publication timed out",
                );
                PublishOutcome::Ambiguous
            }
        }
    }

    /// Records an unexpected panic from one publication attempt.
    pub fn record_panic(&self, prepared: &PreparedTx) {
        self.metrics.record_publish_error();
        error!(
            backend = self.id.index(),
            nonce = prepared.nonce,
            tx_hash = %prepared.tx_hash,
            "transaction publisher panicked",
        );
    }
}

/// Signed version positively acknowledged by one backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcceptedPosition {
    /// Submission occupying the nonce when it was accepted.
    submission_id: SubmissionId,
    /// Signed version accepted at that nonce.
    version: VersionId,
}

/// Signed attempt already completed by one backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptedPosition {
    /// Submission occupying the nonce when it was attempted.
    submission_id: SubmissionId,
    /// Signed version that was attempted.
    version: VersionId,
    /// Coordinator-controlled attempt epoch.
    epoch: u64,
}

/// Per-backend progress through the shared signed ledger.
///
/// Accepted entries form a logical prefix. Completed non-accepted attempts
/// block that backend until the coordinator advances the entry's epoch.
#[derive(Debug, Default)]
pub struct PublisherCursor {
    /// Current versions positively acknowledged by this backend.
    accepted: BTreeMap<u64, AcceptedPosition>,
    /// Current epochs already attempted without acknowledgement.
    attempted: BTreeMap<u64, AttemptedPosition>,
}

impl PublisherCursor {
    /// Returns the next transaction this backend may publish.
    pub fn next(&mut self, snapshot: &PublisherSnapshot) -> Option<PublisherTx> {
        self.prune(snapshot);

        for transaction in snapshot.transactions.iter() {
            let accepted = AcceptedPosition {
                submission_id: transaction.submission_id,
                version: transaction.version,
            };
            match self.accepted.get(&transaction.nonce) {
                Some(current) if *current == accepted => continue,
                Some(_) => self.rewind_from(transaction.nonce),
                None => {}
            }

            let attempted = AttemptedPosition {
                submission_id: transaction.submission_id,
                version: transaction.version,
                epoch: transaction.epoch,
            };
            match self.attempted.get(&transaction.nonce) {
                Some(current) if *current == attempted => return None,
                Some(_) => self.rewind_from(transaction.nonce),
                None => {}
            }
            return Some(transaction.clone());
        }
        None
    }

    /// Records one completed attempt before the worker advances.
    pub fn record(&mut self, transaction: &PublisherTx, outcome: &PublishOutcome) {
        let accepted = AcceptedPosition {
            submission_id: transaction.submission_id,
            version: transaction.version,
        };
        let attempted = AttemptedPosition {
            submission_id: transaction.submission_id,
            version: transaction.version,
            epoch: transaction.epoch,
        };
        match outcome {
            PublishOutcome::Accepted => {
                self.accepted.insert(transaction.nonce, accepted);
                self.attempted.remove(&transaction.nonce);
            }
            PublishOutcome::Ambiguous | PublishOutcome::Rejected(_) => {
                self.attempted.insert(transaction.nonce, attempted);
            }
        }
    }

    /// Forgets the accepted predecessor after a `NonceTooHigh` response.
    pub fn rewind_predecessor(&mut self, snapshot: &PublisherSnapshot, nonce: u64) {
        let Some(predecessor) =
            snapshot.transactions.iter().rev().find(|transaction| transaction.nonce < nonce)
        else {
            return;
        };

        self.accepted.split_off(&predecessor.nonce);
        self.attempted.remove(&predecessor.nonce);
    }

    /// Drops cursor state from an updated or replaced nonce onward.
    pub fn rewind_from(&mut self, nonce: u64) {
        self.accepted.split_off(&nonce);
        self.attempted.split_off(&nonce);
    }

    /// Removes positions no longer present in the current ledger.
    pub fn prune(&mut self, snapshot: &PublisherSnapshot) {
        let nonces = snapshot
            .transactions
            .iter()
            .map(|transaction| transaction.nonce)
            .collect::<BTreeSet<_>>();
        self.accepted.retain(|nonce, _| nonces.contains(nonce));
        self.attempted.retain(|nonce, _| nonces.contains(nonce));
    }
}

/// Symmetric publisher workers fed by coalescing ledger snapshots.
#[derive(Debug, Clone)]
pub struct PublisherGroup {
    /// One latest-snapshot sender per publication backend.
    senders: Arc<[watch::Sender<Arc<PublisherSnapshot>>]>,
}

impl PublisherGroup {
    /// Starts one sequential worker for every backend.
    pub fn new<P, R>(
        providers: Vec<P>,
        runtime: R,
        network_timeout: Duration,
        metrics: Arc<dyn TxMetrics>,
    ) -> (Self, mpsc::UnboundedReceiver<PublisherEvent>)
    where
        P: Provider + Clone + Debug + Send + Sync + 'static,
        R: Runtime,
    {
        let (event_tx, events) = mpsc::unbounded_channel();
        let senders = providers
            .into_iter()
            .enumerate()
            .map(|(index, provider)| {
                let id = PublisherId::new(index);
                let (snapshot_tx, snapshot_rx) =
                    watch::channel(Arc::new(PublisherSnapshot::empty()));
                let publisher = TxPublisher::new(
                    id,
                    provider,
                    runtime.clone(),
                    network_timeout,
                    Arc::clone(&metrics),
                );
                runtime.spawn(Self::run_worker(
                    id,
                    publisher,
                    runtime.clone(),
                    snapshot_rx,
                    event_tx.clone(),
                ));
                snapshot_tx
            })
            .collect::<Vec<_>>()
            .into();
        (Self { senders }, events)
    }

    /// Returns the number of independently progressing backends.
    pub fn len(&self) -> usize {
        self.senders.len()
    }

    /// Returns whether no publication backend is configured.
    pub fn is_empty(&self) -> bool {
        self.senders.is_empty()
    }

    /// Replaces every worker's pending view with the latest ledger snapshot.
    pub fn update(&self, snapshot: PublisherSnapshot) {
        if self.senders.is_empty() {
            return;
        }
        let snapshot = Arc::new(snapshot);
        for sender in self.senders.iter() {
            if sender.borrow().revision != snapshot.revision {
                sender.send_replace(Arc::clone(&snapshot));
            }
        }
    }

    /// Runs one sequential publisher until shutdown or coordinator closure.
    pub async fn run_worker<P, R>(
        id: PublisherId,
        publisher: TxPublisher<P, R>,
        runtime: R,
        mut snapshots: watch::Receiver<Arc<PublisherSnapshot>>,
        events: mpsc::UnboundedSender<PublisherEvent>,
    ) where
        P: Provider + Clone + Debug + Send + Sync + 'static,
        R: Runtime,
    {
        let mut cursor = PublisherCursor::default();

        loop {
            tokio::select! {
                _ = runtime.cancelled() => break,
                changed = snapshots.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
            let snapshot = Arc::clone(&snapshots.borrow_and_update());

            while let Some(transaction) = cursor.next(&snapshot) {
                // Do not start work from an obsolete snapshot. An in-flight
                // request is allowed to finish because cancelling it would make
                // its publication outcome ambiguous.
                match snapshots.has_changed() {
                    Ok(true) => {
                        break;
                    }
                    Ok(false) => {}
                    Err(_) => return,
                }

                let publish =
                    AssertUnwindSafe(publisher.publish(&transaction.prepared)).catch_unwind();
                let outcome = tokio::select! {
                    _ = runtime.cancelled() => break,
                    result = publish => match result {
                        Ok(outcome) => outcome,
                        Err(_) => {
                            publisher.record_panic(&transaction.prepared);
                            PublishOutcome::Ambiguous
                        }
                    }
                };
                let accepted = matches!(outcome, PublishOutcome::Accepted);
                let nonce_too_high =
                    matches!(outcome, PublishOutcome::Rejected(PublishReject::NonceTooHigh));
                cursor.record(&transaction, &outcome);

                let event = PublisherEvent {
                    publisher: id,
                    submission_id: transaction.submission_id,
                    version: transaction.version,
                    epoch: transaction.epoch,
                    kind: transaction.kind,
                    tx_hash: transaction.prepared.tx_hash,
                    outcome,
                };
                if events.send(event).is_err() {
                    return;
                }

                if !accepted {
                    if nonce_too_high {
                        cursor.rewind_predecessor(&snapshot, transaction.nonce);
                    }
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use alloy_provider::{builder as provider_builder, mock::Asserter};
    use base_runtime::{Cancellation, TokioRuntime};

    use super::*;
    use crate::NoopTxMetrics;

    fn prepared(nonce: u64, marker: u8) -> PreparedTx {
        PreparedTx {
            raw_tx: Bytes::from(vec![marker]),
            tx_hash: B256::with_last_byte(marker),
            gas_tip_cap: 1,
            gas_fee_cap: 2,
            blob_fee_cap: None,
            gas_limit: 21_000,
            nonce,
            sidecar: None,
        }
    }

    fn transaction(nonce: u64, version: u32, epoch: u64) -> PublisherTx {
        PublisherTx {
            submission_id: SubmissionId::new(nonce + 1),
            nonce,
            version: VersionId::new(version),
            epoch,
            kind: VersionKind::Original,
            prepared: prepared(nonce, nonce as u8 + version as u8),
        }
    }

    fn snapshot(transactions: Vec<PublisherTx>) -> PublisherSnapshot {
        PublisherSnapshot { revision: 1, transactions: transactions.into() }
    }

    #[test]
    fn accepted_versions_advance_the_backend_cursor() {
        let snapshot = snapshot(vec![transaction(0, 0, 0), transaction(1, 0, 0)]);
        let mut cursor = PublisherCursor::default();

        let first = cursor.next(&snapshot).unwrap();
        cursor.record(&first, &PublishOutcome::Accepted);

        assert_eq!(cursor.next(&snapshot).unwrap().nonce, 1);
    }

    #[test]
    fn ambiguous_attempt_waits_for_a_new_epoch() {
        let mut snapshot = snapshot(vec![transaction(0, 0, 0)]);
        let mut cursor = PublisherCursor::default();

        let first = cursor.next(&snapshot).unwrap();
        cursor.record(&first, &PublishOutcome::Ambiguous);
        assert!(cursor.next(&snapshot).is_none());

        Arc::make_mut(&mut snapshot.transactions)[0].epoch = 1;
        assert_eq!(cursor.next(&snapshot).unwrap().epoch, 1);
    }

    #[test]
    fn version_update_rewinds_that_nonce_and_every_successor() {
        let mut snapshot =
            snapshot(vec![transaction(0, 0, 0), transaction(1, 0, 0), transaction(2, 0, 0)]);
        let mut cursor = PublisherCursor::default();
        for _ in 0..3 {
            let transaction = cursor.next(&snapshot).unwrap();
            cursor.record(&transaction, &PublishOutcome::Accepted);
        }

        Arc::make_mut(&mut snapshot.transactions)[1] = transaction(1, 1, 0);
        let next = cursor.next(&snapshot).unwrap();
        assert_eq!((next.nonce, next.version), (1, VersionId::new(1)));
    }

    #[test]
    fn nonce_too_high_rewinds_to_predecessor() {
        let snapshot = snapshot(vec![transaction(0, 0, 0), transaction(1, 0, 0)]);
        let mut cursor = PublisherCursor::default();
        let predecessor = cursor.next(&snapshot).unwrap();
        cursor.record(&predecessor, &PublishOutcome::Accepted);
        let blocked = cursor.next(&snapshot).unwrap();
        cursor.record(&blocked, &PublishOutcome::Rejected(PublishReject::NonceTooHigh));

        cursor.rewind_predecessor(&snapshot, blocked.nonce);
        let replay = cursor.next(&snapshot).unwrap();
        assert_eq!(replay.nonce, 0);
        cursor.record(&replay, &PublishOutcome::Accepted);
        assert!(cursor.next(&snapshot).is_none());
    }

    #[test]
    fn publication_classification_is_conservative() {
        assert!(matches!(
            PublishOutcome::classify_error(TxManagerError::Transport("redacted".to_string())),
            PublishOutcome::Ambiguous
        ));
        assert!(matches!(
            PublishOutcome::classify_error(TxManagerError::NonceTooHigh),
            PublishOutcome::Rejected(PublishReject::NonceTooHigh)
        ));
        assert!(matches!(
            PublishOutcome::classify_error(TxManagerError::TxPoolFull),
            PublishOutcome::Rejected(PublishReject::Transient(TxManagerError::TxPoolFull))
        ));
    }

    #[test]
    fn rejection_verdict_has_one_priority_order() {
        assert!(matches!(
            PublishReject::verdict(&[
                PublishReject::FeeTooLow(TxManagerError::Underpriced),
                PublishReject::NonceTooHigh,
            ]),
            RejectionVerdict::FeeTooLow(TxManagerError::Underpriced)
        ));
        assert!(matches!(
            PublishReject::verdict(&[
                PublishReject::FeeTooLow(TxManagerError::Underpriced),
                PublishReject::Deterministic(TxManagerError::InsufficientFunds),
            ]),
            RejectionVerdict::Retry(_)
        ));
        assert!(matches!(
            PublishReject::verdict(&[
                PublishReject::Deterministic(TxManagerError::InsufficientFunds),
                PublishReject::Transient(TxManagerError::TxPoolFull),
            ]),
            RejectionVerdict::Retry(_)
        ));
        assert!(matches!(
            PublishReject::verdict(&[
                PublishReject::NonceTooLow,
                PublishReject::Deterministic(TxManagerError::InsufficientFunds),
            ]),
            RejectionVerdict::NonceTooLow
        ));
    }

    #[tokio::test]
    async fn publisher_group_fans_the_same_signed_transaction_to_every_backend() {
        let transaction = transaction(0, 0, 0);
        let first = Asserter::new();
        first.push_success(&transaction.prepared.tx_hash);
        let second = Asserter::new();
        second.push_success(&transaction.prepared.tx_hash);
        let runtime = TokioRuntime::new();
        let (group, mut events) = PublisherGroup::new(
            vec![
                provider_builder().connect_mocked_client(first),
                provider_builder().connect_mocked_client(second),
            ],
            runtime.clone(),
            Duration::from_secs(1),
            Arc::new(NoopTxMetrics),
        );

        group.update(snapshot(vec![transaction.clone()]));
        let mut publishers = BTreeSet::new();
        for _ in 0..2 {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("publisher should finish")
                .expect("publisher event channel should remain open");
            assert_eq!(event.submission_id, transaction.submission_id);
            assert_eq!(event.tx_hash, transaction.prepared.tx_hash);
            assert!(matches!(event.outcome, PublishOutcome::Accepted));
            publishers.insert(event.publisher.index());
        }

        assert_eq!(publishers, BTreeSet::from([0, 1]));
        runtime.cancel();
    }
}
