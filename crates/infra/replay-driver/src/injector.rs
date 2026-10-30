use std::{
    collections::VecDeque,
    future::Future,
    time::{Duration, UNIX_EPOCH},
};

use base_clock::GlobalClock;
use base_replay::{ReplayRecord, ReplayStream};
use tokio_util::sync::CancellationToken;

/// An ingress adapter; implementations select their own replay stream.
#[cfg_attr(test, mockall::automock)]
pub trait Injector: Sync {
    /// Stream accepted by this adapter.
    fn stream(&self) -> ReplayStream;
    /// Delivers one call, reporting terminal failure after bounded transport retries.
    fn inject(
        &self,
        record: ReplayRecord,
    ) -> impl Future<Output = Result<(), InjectionError>> + Send;
}

/// Terminal injection failure. Transport retries belong to the forwarding service.
#[derive(Debug, thiserror::Error)]
pub enum InjectionError {
    /// A record belongs to another ingress boundary.
    #[error("record stream does not match injector")]
    Stream,
    /// A bounded scheduling or delivery wait expired.
    #[error("injection wait timed out")]
    Timeout,
    /// Caller requested cancellation.
    #[error("injection cancelled")]
    Cancelled,
    /// Forwarder stopped before acknowledging delivery.
    #[error("injection destination closed")]
    Closed,
    /// RPC rejected the call or exhausted transport retries.
    #[error(transparent)]
    Rpc(#[from] std::sync::Arc<jsonrpsee::core::ClientError>),
    /// Replay clock predates the Unix epoch.
    #[error(transparent)]
    Clock(#[from] std::time::SystemTimeError),
}

/// Stable timestamp-ordered records, retaining capture order at equal timestamps.
#[derive(Debug)]
pub struct InjectionSchedule {
    records: VecDeque<ReplayRecord>,
    /// Maximum wait for an individual deadline or delivery.
    pub timeout: Duration,
}

impl InjectionSchedule {
    /// Remaining replay delay; late calls are immediately eligible.
    ///
    /// # Errors
    /// Returns a clock error before the Unix epoch.
    pub fn delay(
        received_at_ms: u64,
        now: std::time::SystemTime,
    ) -> Result<Duration, InjectionError> {
        Ok(Duration::from_millis(received_at_ms).saturating_sub(now.duration_since(UNIX_EPOCH)?))
    }

    /// Orders captured calls without changing equal-timestamp batch order.
    pub fn new(mut records: Vec<ReplayRecord>) -> Self {
        records.sort_by_key(|record| record.received_at_ms);
        Self { records: records.into(), timeout: Duration::from_secs(15) }
    }

    /// Delivers every record at or before cutoff immediately, awaiting acceptance before first FCU.
    ///
    /// # Errors
    /// Returns stream, delivery, timeout, or cancellation failures.
    pub async fn warmup(
        &mut self,
        injector: &impl Injector,
        cutoff_ms: u64,
    ) -> Result<(), InjectionError> {
        self.deliver(injector, Some(cutoff_ms)).await
    }

    /// Replays remaining calls against GlobalClock, not elapsed time since this method started.
    ///
    /// # Errors
    /// Stops on stream, clock, RPC, timeout, or cancellation failure.
    /// # Cancel safety
    /// Dropping this future stops scheduling. An unacknowledged call may already have reached the
    /// builder; retrying the retained record has at-least-once semantics.
    pub async fn run(
        &mut self,
        injector: &impl Injector,
        cancel: &CancellationToken,
    ) -> Result<(), InjectionError> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(InjectionError::Cancelled),
            result = self.deliver(injector, None) => result,
        }
    }

    /// Delivers through an optional immediate cutoff, or waits on the global clock without one.
    ///
    /// # Errors
    /// Returns stream, clock, delivery, or timeout failures.
    pub async fn deliver(
        &mut self,
        injector: &impl Injector,
        cutoff: Option<u64>,
    ) -> Result<(), InjectionError> {
        if self.records.iter().any(|record| record.stream != injector.stream()) {
            return Err(InjectionError::Stream);
        }
        while let Some(record) = self.records.front() {
            if let Some(cutoff) = cutoff {
                if record.received_at_ms > cutoff {
                    break;
                }
            } else {
                tokio::time::timeout(self.timeout, async {
                    loop {
                        let wait = Self::delay(record.received_at_ms, GlobalClock::get().now())?;
                        if wait.is_zero() {
                            return Ok::<_, InjectionError>(());
                        }
                        tokio::time::sleep(wait).await;
                    }
                })
                .await
                .map_err(|_| InjectionError::Timeout)??;
            }
            tokio::time::timeout(self.timeout, injector.inject(record.clone()))
                .await
                .map_err(|_| InjectionError::Timeout)??;
            self.records.pop_front();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_offsets_against_clock_when_call_is_future_or_late() {
        let now = UNIX_EPOCH + Duration::from_millis(100);
        assert_eq!(InjectionSchedule::delay(150, now).unwrap(), Duration::from_millis(50));
        assert_eq!(InjectionSchedule::delay(50, now).unwrap(), Duration::ZERO);
    }

    #[tokio::test]
    async fn preserves_capture_order_when_offsets_are_equal() {
        let record = |offset, method: &str| ReplayRecord {
            stream: ReplayStream::BuilderIngress,
            received_at_ms: offset,
            method: method.into(),
            params: serde_json::value::RawValue::from_string("[]".into()).unwrap(),
            batch_id: None,
            source: String::new(),
            tx_hash: None,
        };
        let mut schedule =
            InjectionSchedule::new(vec![record(20, "b"), record(10, "a"), record(20, "c")]);
        let mut injector = MockInjector::new();
        injector.expect_stream().return_const(ReplayStream::BuilderIngress);
        let mut sequence = mockall::Sequence::new();
        for method in ["a", "b", "c"] {
            injector
                .expect_inject()
                .withf(move |r| r.method == method)
                .once()
                .in_sequence(&mut sequence)
                .returning(|_| Box::pin(async { Ok(()) }));
        }
        schedule.warmup(&injector, 10).await.unwrap();
        schedule.warmup(&injector, 20).await.unwrap();
    }
}
