//! Hybrid L1 head source that races a subscription stream against interval-based polling.

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use tokio::time::{Duration, Interval, interval};

use crate::{L1HeadEvent, L1HeadPolling, L1HeadSource, L1HeadSubscription, SourceError};

/// An L1 head source that races a subscription stream against an interval-based poller.
///
/// Deduplicates head numbers so that the same block number is only reported once.
/// Stale reads (same or lower block number than last reported) are also silently dropped.
#[derive(derive_more::Debug)]
pub struct HybridL1HeadSource<S, P> {
    /// The head number stream returned by `S::take_stream`.
    ///
    /// Declared before `_subscription` so it is dropped first, ensuring the
    /// stream's underlying transport is released before the provider is torn down.
    #[debug(skip)]
    sub: BoxStream<'static, Result<u64, SourceError>>,
    /// The original subscription, kept alive so its resources remain open.
    #[debug(skip)]
    _subscription: S,
    /// Polling source for fetching the latest L1 head block number.
    #[debug(skip)]
    poller: P,
    /// Polling interval timer.
    #[debug(skip)]
    interval: Interval,
    /// Last reported head number for deduplication.
    last_head: Option<u64>,
}

impl<S, P> HybridL1HeadSource<S, P>
where
    S: L1HeadSubscription,
    P: L1HeadPolling,
{
    /// Create a new hybrid L1 head source.
    ///
    /// Calls [`L1HeadSubscription::take_stream`] once to obtain the live head
    /// number stream, then retains the subscription to keep any underlying
    /// resources (e.g. a WebSocket provider) alive. Combines the stream with a
    /// poller that fires at `poll_interval`.
    pub fn new(mut subscription: S, poller: P, poll_interval: Duration) -> Self {
        let sub = subscription.take_stream();
        Self {
            sub,
            _subscription: subscription,
            poller,
            interval: interval(poll_interval),
            last_head: None,
        }
    }

    /// Process a received head number, returning an event if it is strictly newer.
    ///
    /// Drops duplicate or stale values (same or lower head number than last emitted).
    fn process(&mut self, head: u64) -> Option<L1HeadEvent> {
        if self.last_head.is_some_and(|last| last >= head) {
            tracing::debug!(head, "stale or duplicate L1 head, skipping");
            return None;
        }
        self.last_head = Some(head);
        Some(L1HeadEvent::NewHead(head))
    }
}

#[async_trait]
impl<S, P> L1HeadSource for HybridL1HeadSource<S, P>
where
    S: L1HeadSubscription,
    P: L1HeadPolling,
{
    async fn next(&mut self) -> Result<L1HeadEvent, SourceError> {
        loop {
            tokio::select! {
                head = self.sub.next() => {
                    match head {
                        Some(Ok(n)) => {
                            if let Some(event) = self.process(n) {
                                return Ok(event);
                            }
                            // Stale or duplicate — loop for next event.
                        }
                        Some(Err(e)) => return Err(e),
                        None => return Err(SourceError::Closed),
                    }
                }
                _ = self.interval.tick() => {
                    match self.poller.latest_head().await {
                        Ok(n) => {
                            if let Some(event) = self.process(n) {
                                return Ok(event);
                            }
                            // Stale or duplicate — loop for next event.
                        }
                        Err(SourceError::Provider(msg)) => {
                            tracing::warn!(error = %msg, "L1 head polling error, retrying on next tick");
                            // Transient provider error — continue to next tick.
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use futures::{StreamExt, stream::BoxStream};
    use tokio::time::Duration;

    use super::*;

    struct StreamSub(BoxStream<'static, Result<u64, SourceError>>);

    impl L1HeadSubscription for StreamSub {
        fn take_stream(&mut self) -> BoxStream<'static, Result<u64, SourceError>> {
            std::mem::replace(&mut self.0, futures::stream::pending().boxed())
        }
    }

    struct FixedPoller(u64);

    #[async_trait]
    impl L1HeadPolling for FixedPoller {
        async fn latest_head(&self) -> Result<u64, SourceError> {
            Ok(self.0)
        }
    }

    #[tokio::test]
    async fn test_hybrid_l1_new_head() {
        let stream = futures::stream::once(async { Ok(5u64) });
        let mut source = HybridL1HeadSource::new(
            StreamSub(stream.boxed()),
            FixedPoller(10),
            Duration::from_secs(100),
        );

        let event = source.next().await.unwrap();
        assert_eq!(event, L1HeadEvent::NewHead(5));
    }

    #[tokio::test]
    async fn test_hybrid_l1_duplicate_skipped() {
        let stream = futures::stream::iter(vec![Ok(5u64), Ok(5u64)]);
        let mut source = HybridL1HeadSource::new(
            StreamSub(stream.boxed()),
            FixedPoller(5),
            Duration::from_secs(100),
        );

        let event = source.next().await.unwrap();
        assert_eq!(event, L1HeadEvent::NewHead(5));

        // Second identical value is skipped; stream exhausted → Closed.
        let err = source.next().await.unwrap_err();
        assert!(matches!(err, SourceError::Closed));
    }

    #[tokio::test]
    async fn test_hybrid_l1_stale_dropped() {
        // Deliver 10, then 9 (stale), then stream closes.
        let stream = futures::stream::iter(vec![Ok(10u64), Ok(9u64)]);
        let mut source = HybridL1HeadSource::new(
            StreamSub(stream.boxed()),
            FixedPoller(3),
            Duration::from_secs(100),
        );

        let event = source.next().await.unwrap();
        assert_eq!(event, L1HeadEvent::NewHead(10));

        // 9 < 10 — stale, skipped. Stream exhausted → Closed.
        let err = source.next().await.unwrap_err();
        assert!(matches!(err, SourceError::Closed));
    }

    #[tokio::test]
    async fn test_hybrid_l1_stream_error() {
        let stream =
            futures::stream::once(async { Err(SourceError::Provider("rpc down".to_string())) });
        let mut source = HybridL1HeadSource::new(
            StreamSub(stream.boxed()),
            FixedPoller(1),
            Duration::from_secs(100),
        );

        let err = source.next().await.unwrap_err();
        assert!(matches!(err, SourceError::Provider(_)));
    }
}
