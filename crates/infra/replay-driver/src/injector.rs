use std::{
    collections::VecDeque,
    future::Future,
    time::{Duration, UNIX_EPOCH},
};

use base_clock::GlobalClock;
use base_replay::{ReplayRecord, ReplayStream};
use eyre::{Result, ensure};
use tokio_util::sync::CancellationToken;

/// An ingress adapter; implementations select their own replay stream.
#[cfg_attr(test, mockall::automock)]
pub trait Injector: Sync {
    /// Stream accepted by this adapter.
    fn stream(&self) -> ReplayStream;
    /// Delivers one call, reporting terminal failure after bounded transport retries.
    fn inject(&self, record: ReplayRecord) -> impl Future<Output = Result<()>> + Send;
}

/// Stable timestamp-ordered records, retaining capture order at equal timestamps.
#[derive(Debug)]
pub struct InjectionSchedule {
    records: VecDeque<ReplayRecord>,
    /// Maximum duration of one delivery; recorded waiting time is not a delivery timeout.
    pub timeout: Duration,
}

impl InjectionSchedule {
    /// Remaining delay, saturating when late; errors before the Unix epoch.
    pub fn delay(received_at_ms: u64, now: std::time::SystemTime) -> Result<Duration> {
        Ok(Duration::from_millis(received_at_ms).saturating_sub(now.duration_since(UNIX_EPOCH)?))
    }

    /// Orders captured calls without changing equal-timestamp batch order.
    pub fn new(mut records: Vec<ReplayRecord>) -> Self {
        records.sort_by_key(|record| record.received_at_ms);
        Self { records: records.into(), timeout: Duration::from_secs(15) }
    }

    /// Immediately delivers through cutoff before first FCU; propagates delivery/timeout errors.
    pub async fn warmup(&mut self, injector: &impl Injector, cutoff_ms: u64) -> Result<()> {
        self.deliver(injector, Some(cutoff_ms)).await
    }

    /// Replays remaining calls against `GlobalClock`, not elapsed time since this method started.
    /// Stops on stream, clock, RPC, timeout, or cancellation failure.
    /// # Cancel safety
    /// Dropping this future stops scheduling. An unacknowledged call may already have reached the
    /// builder; retrying the retained record has at-least-once semantics.
    pub async fn run(
        &mut self,
        injector: &impl Injector,
        cancel: &CancellationToken,
    ) -> Result<()> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => eyre::bail!("injection cancelled"),
            result = self.deliver(injector, None) => result,
        }
    }

    /// Delivers through an optional immediate cutoff, or waits on the global clock without one.
    /// Propagates stream, clock, delivery, and timeout errors.
    pub async fn deliver(&mut self, injector: &impl Injector, cutoff: Option<u64>) -> Result<()> {
        ensure!(
            self.records.iter().all(|record| record.stream == injector.stream()),
            "record stream does not match injector"
        );
        while let Some(record) = self.records.front() {
            if let Some(cutoff) = cutoff {
                if record.received_at_ms > cutoff {
                    break;
                }
            } else {
                loop {
                    let wait = Self::delay(record.received_at_ms, GlobalClock::get().now())?;
                    if wait.is_zero() {
                        break;
                    }
                    tokio::time::sleep(wait).await;
                }
            }
            tokio::time::timeout(self.timeout, injector.inject(record.clone())).await??;
            self.records.pop_front();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn sparse_prefix_and_large_inter_record_gap_preserve_deadlines() {
        #[derive(Debug)]
        struct VirtualClock(tokio::time::Instant);
        impl base_clock::Clock for VirtualClock {
            fn now(&self) -> std::time::SystemTime {
                UNIX_EPOCH + self.0.elapsed()
            }
        }
        GlobalClock::set(VirtualClock(tokio::time::Instant::now())).unwrap();
        let records = [100_000, 300_000].map(|ms| serde_json::from_str(&format!(
            r#"{{"stream":"builder_ingress","received_at_ms":{ms},"method":"call","params":[],"source":"test"}}"#
        )).unwrap());
        let mut injector = MockInjector::new();
        injector.expect_stream().return_const(ReplayStream::BuilderIngress);
        injector.expect_inject().times(2).returning(|record| {
            assert_eq!(
                GlobalClock::get().now_unix_millis().unwrap(),
                u128::from(record.received_at_ms)
            );
            Box::pin(async { Ok(()) })
        });
        let mut schedule = InjectionSchedule::new(records.into());

        schedule.run(&injector, &CancellationToken::new()).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn delivery_still_times_out_when_injector_never_acknowledges() {
        let record = serde_json::from_str(
            r#"{"stream":"builder_ingress","received_at_ms":0,"method":"call","params":[],"source":"test"}"#,
        ).unwrap();
        let mut injector = MockInjector::new();
        injector.expect_stream().return_const(ReplayStream::BuilderIngress);
        injector.expect_inject().once().returning(|_| Box::pin(std::future::pending()));
        let mut schedule = InjectionSchedule::new(vec![record]);
        schedule.timeout = Duration::from_millis(1);

        assert!(schedule.run(&injector, &CancellationToken::new()).await.is_err());
    }

    #[test]
    fn computes_offsets_against_clock_when_call_is_future_or_late() {
        let now = UNIX_EPOCH + Duration::from_millis(100);
        assert_eq!(InjectionSchedule::delay(150, now).unwrap(), Duration::from_millis(50));
        assert_eq!(InjectionSchedule::delay(50, now).unwrap(), Duration::ZERO);
    }

    #[tokio::test]
    async fn preserves_capture_order_when_offsets_are_equal() {
        let record = |offset, method: &str| {
            serde_json::from_str(&format!(
            r#"{{"stream":"builder_ingress","received_at_ms":{offset},"method":"{method}","params":[],"source":""}}"#
        )).unwrap()
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
