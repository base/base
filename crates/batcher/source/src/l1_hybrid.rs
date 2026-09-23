//! Hybrid L1 head source that races a subscription stream against interval-based polling.

use std::time::Duration;

use async_trait::async_trait;
use base_runtime::Clock;
use futures::{StreamExt, stream::BoxStream};

use crate::{L1HeadPolling, L1HeadSource};

/// An L1 head source that races a subscription stream against an interval-based poller.
///
/// Deduplicates head numbers so that the same block number is only reported once.
/// Stale reads (same or lower block number than last reported) are also silently dropped.
///
/// When the subscription stream ends, the source keeps delivering heads from the poller.
#[derive(derive_more::Debug)]
pub struct HybridL1HeadSource<P> {
    /// Live head numbers pushed by the subscription; pending forever once it ends.
    #[debug(skip)]
    sub: BoxStream<'static, u64>,
    /// Polling source for fetching the latest L1 head block number.
    #[debug(skip)]
    poller: P,
    /// Polling interval timer.
    #[debug(skip)]
    interval: BoxStream<'static, ()>,
    /// Last reported head number for deduplication.
    last_head: Option<u64>,
}

impl<P: L1HeadPolling> HybridL1HeadSource<P> {
    /// Create a new hybrid L1 head source.
    ///
    /// `sub` carries the live head numbers and must own whatever keeps them flowing
    /// (for example a WebSocket provider). `poller` is queried for the latest head on every
    /// `poll_interval` tick.
    pub fn new(
        clock: impl Clock,
        sub: BoxStream<'static, u64>,
        poller: P,
        poll_interval: Duration,
    ) -> Self {
        Self { sub, poller, interval: clock.interval(poll_interval), last_head: None }
    }

    /// Record a received head number, returning it if it is strictly newer.
    ///
    /// Drops duplicate or stale values (same or lower head number than last emitted).
    fn process(&mut self, head: u64) -> Option<u64> {
        if self.last_head.is_some_and(|last| last >= head) {
            tracing::debug!(head, "stale or duplicate L1 head, skipping");
            return None;
        }
        self.last_head = Some(head);
        Some(head)
    }
}

#[async_trait]
impl<P: L1HeadPolling> L1HeadSource for HybridL1HeadSource<P> {
    async fn next(&mut self) -> u64 {
        loop {
            tokio::select! {
                next = self.sub.next() => {
                    match next {
                        Some(head) => {
                            if let Some(head) = self.process(head) {
                                return head;
                            }
                            // Stale or duplicate: loop for the next one.
                        }
                        None => {
                            tracing::warn!("L1 head subscription ended; falling back to polling");
                            self.sub = futures::stream::pending().boxed();
                        }
                    }
                }
                _ = self.interval.next() => {
                    match self.poller.latest_head().await {
                        Ok(head) => {
                            if let Some(head) = self.process(head) {
                                return head;
                            }
                            // Stale or duplicate: loop for the next one.
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "L1 head polling error, retrying on next tick");
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use base_runtime::{Config, Runner};

    use super::*;
    use crate::SourceError;

    struct IncrementingPoller(AtomicU64);

    #[async_trait]
    impl L1HeadPolling for IncrementingPoller {
        async fn latest_head(&self) -> Result<u64, SourceError> {
            Ok(self.0.fetch_add(1, Ordering::Relaxed))
        }
    }

    struct ProviderErrorPoller;

    #[async_trait]
    impl L1HeadPolling for ProviderErrorPoller {
        async fn latest_head(&self) -> Result<u64, SourceError> {
            Err(SourceError::Provider("poll down".to_string()))
        }
    }

    #[test]
    fn test_hybrid_l1_stale_and_duplicate_heads_skipped() {
        Runner::start(Config::seeded(0), |ctx| async move {
            // Only the stream can produce heads: the poller always fails.
            let stream = futures::stream::iter(vec![10u64, 9, 10, 11]);
            let mut source = HybridL1HeadSource::new(
                ctx,
                stream.boxed(),
                ProviderErrorPoller,
                Duration::from_secs(100),
            );

            assert_eq!(source.next().await, 10);
            // 9 is stale and the second 10 a duplicate: the next head is 11.
            assert_eq!(source.next().await, 11);
        });
    }

    #[test]
    fn test_hybrid_l1_polls_after_subscription_ends() {
        Runner::start(Config::seeded(0), |ctx| async move {
            // The stream and the poller both start at head 5; the stream then ends and the
            // poller keeps returning a new head on every call. Whichever arm `select!` polls
            // first, the other one's 5 is a duplicate and the heads that follow come from the
            // poller.
            let stream = futures::stream::once(async { 5u64 });
            let mut source = HybridL1HeadSource::new(
                ctx,
                stream.boxed(),
                IncrementingPoller(AtomicU64::new(5)),
                Duration::from_secs(100),
            );

            let mut heads = Vec::new();
            for _ in 0..3 {
                heads.push(source.next().await);
            }
            assert_eq!(heads, [5, 6, 7]);
        });
    }
}
