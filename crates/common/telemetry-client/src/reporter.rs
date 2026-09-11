//! The bounded queue and background delivery task in front of a [`ReportSink`].

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use backon::Retryable;
use base_retry::RetryConfig;
use base_telemetry_types::NodeReport;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{Metrics, ReportSink};

/// Hands reports to a sink without ever blocking the caller.
///
/// The queue is bounded and lossy. A caller that cannot enqueue drops the report and increments
/// a counter rather than waiting, which is the same discipline the transaction event writer
/// uses: a telemetry outage must never degrade a node.
#[derive(Debug, Clone)]
pub struct TelemetryReporter {
    sender: mpsc::Sender<NodeReport>,
    dropped: Arc<AtomicU64>,
}

/// Tracks a run of consecutive delivery failures.
///
/// A node pointed at an endpoint that never comes back should stay quiet enough to run for
/// weeks, so only the first failure of an outage is worth a warning. This decides which one
/// that is, and how many failures a recovery ended.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryStreak {
    consecutive_failures: u64,
}

impl DeliveryStreak {
    /// Creates a streak with no failures recorded.
    pub const fn new() -> Self {
        Self { consecutive_failures: 0 }
    }

    /// Records a failed delivery, returning whether it opened a new outage.
    ///
    /// Only the opening failure warrants a warning; the rest of the outage belongs at debug.
    pub const fn record_failure(&mut self) -> bool {
        self.consecutive_failures += 1;
        self.consecutive_failures == 1
    }

    /// Records a successful delivery, returning how many consecutive failures it ended.
    ///
    /// Zero means delivery was already healthy and the success is unremarkable.
    pub const fn record_success(&mut self) -> u64 {
        let ended = self.consecutive_failures;
        self.consecutive_failures = 0;
        ended
    }

    /// Returns how many deliveries have failed in a row.
    pub const fn consecutive_failures(&self) -> u64 {
        self.consecutive_failures
    }
}

impl TelemetryReporter {
    /// Starts the background delivery task and returns a handle for enqueueing reports.
    ///
    /// The task exits when `cancellation` fires or when every handle is dropped.
    pub fn spawn(
        sink: Arc<dyn ReportSink>,
        retry: RetryConfig,
        queue_capacity: usize,
        cancellation: CancellationToken,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(queue_capacity.max(1));
        tokio::spawn(Self::deliver(sink, retry, receiver, cancellation));
        Self { sender, dropped: Arc::new(AtomicU64::new(0)) }
    }

    /// Queues a report for delivery, returning whether it was accepted.
    ///
    /// Never blocks and never awaits. A full queue means delivery is wedged, and in that state
    /// the useful thing to keep is the newest report rather than a backlog of stale ones.
    pub fn enqueue(&self, report: NodeReport) -> bool {
        Metrics::reports_enqueued().increment(1);
        if self.sender.try_send(report).is_ok() {
            return true;
        }

        let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        Metrics::reports_dropped().increment(1);
        warn!(
            target: "telemetry",
            dropped_total = dropped,
            "telemetry queue is full; dropping report"
        );
        false
    }

    /// Returns how many reports have been dropped for lack of queue space.
    pub fn dropped_reports(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Drains the queue, delivering each report with backoff.
    async fn deliver(
        sink: Arc<dyn ReportSink>,
        retry: RetryConfig,
        mut receiver: mpsc::Receiver<NodeReport>,
        cancellation: CancellationToken,
    ) {
        let mut streak = DeliveryStreak::new();
        loop {
            let report = tokio::select! {
                _ = cancellation.cancelled() => return,
                report = receiver.recv() => match report {
                    Some(report) => report,
                    None => return,
                },
            };

            let delivery = (|| async { sink.send(&report).await })
                .retry(retry.to_backoff_builder())
                .when(|error| error.is_retryable())
                .notify(|error, delay| {
                    debug!(
                        target: "telemetry",
                        backoff_ms = delay.as_millis(),
                        error_kind = error.kind(),
                        "telemetry delivery failed; retrying"
                    );
                });

            let outcome = tokio::select! {
                _ = cancellation.cancelled() => return,
                outcome = delivery => outcome,
            };

            match outcome {
                Ok(()) => {
                    Metrics::reports_sent().increment(1);
                    let ended = streak.record_success();
                    if ended > 0 {
                        warn!(
                            target: "telemetry",
                            failed_reports = ended,
                            "telemetry delivery recovered"
                        );
                    }
                    debug!(target: "telemetry", "report delivered");
                }
                Err(error) => {
                    Metrics::reports_failed().increment(1);
                    if streak.record_failure() {
                        warn!(
                            target: "telemetry",
                            error = %error,
                            error_kind = error.kind(),
                            "giving up on telemetry report; further failures log at debug until \
                             delivery recovers"
                        );
                    } else {
                        debug!(
                            target: "telemetry",
                            error = %error,
                            error_kind = error.kind(),
                            consecutive_failures = streak.consecutive_failures(),
                            "giving up on telemetry report"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use base_telemetry_types::NODE_REPORT_SCHEMA_VERSION;
    use tokio::sync::{mpsc as test_mpsc, oneshot};
    use uuid::Uuid;

    use super::*;
    use crate::{MockReportSink, ReportSinkError};

    /// A fixed identity, so a delivered report can be matched back to the one that was enqueued.
    const TELEMETRY_ID: Uuid = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);

    fn report() -> NodeReport {
        NodeReport {
            schema_version: NODE_REPORT_SCHEMA_VERSION,
            telemetry_id: TELEMETRY_ID,
            ..Default::default()
        }
    }

    fn fast_retry() -> RetryConfig {
        RetryConfig::new(2, Duration::from_millis(1), Duration::from_millis(2))
    }

    #[tokio::test]
    async fn test_enqueued_reports_reach_the_sink() {
        let (tx, mut rx) = test_mpsc::channel(4);
        let mut sink = MockReportSink::new();
        sink.expect_send().returning(move |report| {
            let tx = tx.clone();
            let id = report.telemetry_id;
            Box::pin(async move {
                tx.try_send(id).expect("test channel should accept");
                Ok(())
            })
        });

        let reporter =
            TelemetryReporter::spawn(Arc::new(sink), fast_retry(), 4, CancellationToken::new());
        assert!(reporter.enqueue(report()));

        let delivered = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("delivery should not time out")
            .expect("sink should have been called");
        assert_eq!(delivered, TELEMETRY_ID);
    }

    #[tokio::test]
    async fn test_a_full_queue_drops_rather_than_blocking() {
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let release_rx = std::sync::Mutex::new(Some(release_rx));

        let mut sink = MockReportSink::new();
        sink.expect_send().returning(move |_| {
            // Wedge the first delivery so the queue backs up behind the in-flight report.
            let gate = release_rx.lock().expect("release lock should not be poisoned").take();
            Box::pin(async move {
                if let Some(gate) = gate {
                    let _ = gate.await;
                }
                Ok(())
            })
        });

        let reporter =
            TelemetryReporter::spawn(Arc::new(sink), fast_retry(), 1, CancellationToken::new());

        // At most one report can be in flight and one queued. Everything after that is dropped,
        // and the point of the test is that every `enqueue` returns immediately either way.
        let accepted = (0..16).filter(|_| reporter.enqueue(report())).count();
        assert!(accepted < 16, "a wedged sink must not let the queue grow without bound");
        assert!(reporter.dropped_reports() > 0, "drops must be counted, not silent");

        let _ = release_tx.send(());
    }

    #[tokio::test]
    async fn test_delivery_retries_a_retryable_failure() {
        let (tx, mut rx) = test_mpsc::channel(8);
        let mut sink = MockReportSink::new();
        sink.expect_send()
            .times(1)
            .returning(|_| Box::pin(async { Err(ReportSinkError::Status { status: 503 }) }));
        sink.expect_send().returning(move |_| {
            let tx = tx.clone();
            Box::pin(async move {
                tx.try_send(()).expect("test channel should accept");
                Ok(())
            })
        });

        let reporter =
            TelemetryReporter::spawn(Arc::new(sink), fast_retry(), 4, CancellationToken::new());
        reporter.enqueue(report());

        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the retry should not time out")
            .expect("the second attempt should have succeeded");
    }

    #[tokio::test]
    async fn test_a_permanent_failure_is_not_retried_and_does_not_stop_the_task() {
        let (tx, mut rx) = test_mpsc::channel(4);
        let mut sink = MockReportSink::new();
        sink.expect_send()
            .times(1)
            .returning(|_| Box::pin(async { Err(ReportSinkError::Status { status: 400 }) }));
        sink.expect_send().returning(move |_| {
            let tx = tx.clone();
            Box::pin(async move {
                tx.try_send(()).expect("test channel should accept");
                Ok(())
            })
        });

        let reporter =
            TelemetryReporter::spawn(Arc::new(sink), fast_retry(), 4, CancellationToken::new());
        reporter.enqueue(report());
        reporter.enqueue(report());

        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the second report should still be delivered")
            .expect("the delivery task must survive a permanent failure");
    }

    #[tokio::test]
    async fn test_cancellation_stops_the_delivery_task() {
        let mut sink = MockReportSink::new();
        sink.expect_send().returning(|_| Box::pin(async { Ok(()) }));

        let cancellation = CancellationToken::new();
        let reporter =
            TelemetryReporter::spawn(Arc::new(sink), fast_retry(), 4, cancellation.clone());
        cancellation.cancel();
        tokio::task::yield_now().await;

        // Enqueueing after cancellation must still not block or panic, only fill and drop.
        for _ in 0..16 {
            reporter.enqueue(report());
        }
    }

    /// Only the first failure of an outage is loud.
    ///
    /// A node pointed at a dead endpoint reports on every cycle forever. Warning on each one
    /// fills an operator's logs with a problem they were already told about once.
    #[test]
    fn test_only_the_first_failure_of_an_outage_warrants_a_warning() {
        let mut streak = DeliveryStreak::new();

        assert!(streak.record_failure(), "the first failure opens the outage");
        for cycle in 2..=100 {
            assert!(!streak.record_failure(), "cycle {cycle} must stay quiet");
        }
        assert_eq!(streak.consecutive_failures(), 100);
    }

    /// A recovery reports the outage it ended, and reopens the next one loudly.
    #[test]
    fn test_recovery_reports_the_outage_it_ended_and_rearms_the_warning() {
        let mut streak = DeliveryStreak::new();
        streak.record_failure();
        streak.record_failure();

        assert_eq!(streak.record_success(), 2, "recovery reports how many reports were lost");
        assert_eq!(streak.consecutive_failures(), 0);
        assert!(streak.record_failure(), "the next outage is worth a warning of its own");
    }

    /// A success during healthy delivery says nothing.
    #[test]
    fn test_a_success_without_a_preceding_failure_is_unremarkable() {
        let mut streak = DeliveryStreak::new();
        assert_eq!(streak.record_success(), 0);
        assert_eq!(streak.record_success(), 0);
    }
}
