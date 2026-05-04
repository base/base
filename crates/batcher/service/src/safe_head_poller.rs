//! Background poller that keeps the safe L2 head watch channel up to date.

use std::{future::Future, time::Duration};

use base_runtime::{Runtime, TokioRuntime};
use tokio::sync::watch;
use tracing::warn;

/// Fetches the current safe L2 head block number from the rollup node.
///
/// The canonical implementation delegates to
/// [`RollupNodeApiClient::sync_status`](base_consensus_rpc::RollupNodeApiClient).
pub trait SafeHeadProvider: Send + Sync + 'static {
    /// Return the current safe L2 head block number.
    fn safe_l2_number(
        &self,
    ) -> impl Future<Output = Result<u64, Box<dyn std::error::Error + Send + Sync>>> + Send + '_;
}

impl SafeHeadProvider for jsonrpsee::http_client::HttpClient {
    async fn safe_l2_number(&self) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        use base_consensus_rpc::RollupNodeApiClient;
        let status = self.sync_status().await?;
        Ok(status.safe_l2.block_info.number)
    }
}

/// Polls a [`SafeHeadProvider`] at a fixed interval and advances a watch
/// channel when the safe L2 head moves forward.
///
/// The poller waits `poll_interval` before the first call, then loops.
/// When the safe head advances, it calls [`watch::Sender::send_if_modified`]
/// so receivers are only woken when the value actually changes.
///
/// Stops cleanly when the runtime passed to [`run`](Self::run) is cancelled.
/// At most one in-flight RPC call is waited for before exit.
#[derive(Debug)]
pub struct SafeHeadPoller<C: SafeHeadProvider> {
    provider: C,
    poll_interval: Duration,
    safe_head_tx: watch::Sender<u64>,
}

impl<C: SafeHeadProvider> SafeHeadPoller<C> {
    /// Create a new [`SafeHeadPoller`].
    pub const fn new(
        provider: C,
        poll_interval: Duration,
        safe_head_tx: watch::Sender<u64>,
    ) -> Self {
        Self { provider, poll_interval, safe_head_tx }
    }

    /// Run the polling loop until `cancellation` fires.
    ///
    /// Cancellation is checked before every sleep, so the poller exits within
    /// one poll interval of the runtime being cancelled.
    pub async fn run<R: Runtime>(self, runtime: R) {
        loop {
            tokio::select! {
                biased;
                _ = runtime.cancelled() => break,
                _ = runtime.sleep(self.poll_interval) => {}
            }
            match self.provider.safe_l2_number().await {
                Ok(n) => {
                    self.safe_head_tx.send_if_modified(|old| {
                        if n > *old {
                            *old = n;
                            true
                        } else {
                            false
                        }
                    });
                }
                Err(e) => {
                    warn!(error = %e, "failed to poll optimism_syncStatus for safe head");
                }
            }
        }
    }

    /// Spawn the polling loop as a background tokio task.
    pub fn spawn(self, runtime: TokioRuntime) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run(runtime))
    }

    /// Spawn the polling loop on an injected runtime.
    pub fn spawn_with_runtime<R: Runtime>(self, runtime: R) {
        let task_runtime = runtime.clone();
        std::mem::drop(runtime.spawn(self.run(task_runtime)));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use base_runtime::{
        Cancellation, Clock, Spawner,
        deterministic::{Config, Runner},
    };
    use tokio::sync::watch;

    use super::{SafeHeadPoller, SafeHeadProvider};

    // ---- Mock providers ----

    /// Returns values from a pre-loaded queue; returns `0` when exhausted.
    struct MockProvider {
        values: Arc<Mutex<Vec<u64>>>,
    }

    impl SafeHeadProvider for MockProvider {
        async fn safe_l2_number(&self) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
            let mut v = self.values.lock().unwrap();
            Ok(if v.is_empty() { 0 } else { v.remove(0) })
        }
    }

    /// Always returns an error.
    struct ErrorProvider;

    impl SafeHeadProvider for ErrorProvider {
        async fn safe_l2_number(&self) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
            Err("rpc error".into())
        }
    }

    // ---- Tests ----

    /// When the provider returns a higher block number, the watch channel must
    /// be updated and receivers notified.
    #[test]
    fn poll_advances_watch_channel() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (tx, mut rx) = watch::channel(0u64);
            let provider = MockProvider { values: Arc::new(Mutex::new(vec![5, 10])) };

            let poller = SafeHeadPoller::new(provider, Duration::from_secs(1), tx);
            let handle = ctx.spawn(poller.run(ctx.clone()));

            rx.changed().await.expect("sender should still be alive");

            ctx.cancel();
            handle.await.expect("poller task should stop");

            assert_eq!(*rx.borrow(), 5, "safe head must advance to first provider value");
        });
    }

    /// When the cancellation token fires, the poller must exit within one poll
    /// interval. It must not leak as a background task.
    #[test]
    fn cancellation_stops_poller() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (tx, _rx) = watch::channel(0u64);
            let provider = MockProvider { values: Arc::new(Mutex::new(vec![])) };

            let poller = SafeHeadPoller::new(provider, Duration::from_secs(50), tx);
            let handle = ctx.spawn(poller.run(ctx.clone()));

            ctx.cancel();

            handle.await.expect("poller must stop after cancellation");
        });
    }

    /// Provider errors must be logged and swallowed — the poller must keep
    /// running and not advance the watch channel.
    #[test]
    fn provider_errors_are_non_fatal() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (tx, rx) = watch::channel(0u64);

            let poller = SafeHeadPoller::new(ErrorProvider, Duration::from_secs(1), tx);
            let handle = ctx.spawn(poller.run(ctx.clone()));

            ctx.sleep(Duration::from_secs(3)).await;
            ctx.cancel();
            handle.await.expect("poller task should stop");

            assert_eq!(*rx.borrow(), 0, "watch must not advance when provider errors");
        });
    }

    /// When the provider returns the same or lower value, `send_if_modified`
    /// must not notify receivers. Check while the poller is still running
    /// (sender alive) so a dropped-sender signal cannot mask a missing change.
    #[test]
    fn watch_not_notified_when_value_unchanged() {
        Runner::start(Config::seeded(0), |ctx| async move {
            let (tx, mut rx) = watch::channel(10u64);
            // Mark the initial value as seen so `changed()` only fires on a new send.
            let _ = rx.borrow_and_update();

            // MockProvider with no queued values returns 0, which is < 10 (initial),
            // so send_if_modified will always return false.
            let provider = MockProvider { values: Arc::new(Mutex::new(vec![])) };

            let poller = SafeHeadPoller::new(provider, Duration::from_secs(1), tx);
            let handle = ctx.spawn(poller.run(ctx.clone()));
            let timeout_ctx = ctx.clone();

            tokio::select! {
                changed = rx.changed() => {
                    panic!("watch fired without advancement: {changed:?}");
                }
                _ = timeout_ctx.sleep(Duration::from_secs(3)) => {}
            }

            ctx.cancel();
            handle.await.expect("poller task should stop");
        });
    }
}
