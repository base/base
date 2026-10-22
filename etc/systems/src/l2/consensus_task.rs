//! Bounded cancellation for in-process consensus services, including startup.

use std::{future::Future, time::Duration};

use tokio::task::JoinHandle;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::warn;

/// Owns a consensus service that must stop even if it does not observe cancellation.
///
/// Dropping the owner requests cancellation. The supervisor gives the service five seconds to
/// finish before dropping its future and the resources it owns. This also covers startup errors
/// and cancellation of the caller's startup or shutdown future.
#[derive(Debug)]
pub struct ConsensusTask {
    handle: JoinHandle<()>,
    cancel_on_drop: DropGuard,
}

impl ConsensusTask {
    /// Maximum time for a service to cooperate with cancellation before it is dropped.
    pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

    /// Spawns and supervises the entire service future, not only its running actors.
    pub fn spawn(
        cancellation: CancellationToken,
        service: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        let cancel_on_drop = cancellation.clone().drop_guard();
        let handle = tokio::spawn(async move {
            tokio::pin!(service);
            tokio::select! {
                () = &mut service => {}
                () = cancellation.cancelled() => {
                    if tokio::time::timeout(Self::SHUTDOWN_GRACE, &mut service).await.is_err() {
                        warn!("forcing in-process consensus shutdown after grace period");
                    }
                }
            }
            // Drop the service here even when it is stuck in setup or ignores cancellation.
        });
        Self { handle, cancel_on_drop }
    }

    /// Requests cancellation and waits until the service future and its resources are dropped.
    pub async fn shutdown(self) {
        drop(self.cancel_on_drop);
        let _ = self.handle.await;
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test]
    async fn shutdown_allows_cooperative_cleanup() {
        let cancellation = CancellationToken::new();
        let node_cancellation = cancellation.clone();
        let (finished, cleanup) = oneshot::channel();
        let task = ConsensusTask::spawn(cancellation, async move {
            node_cancellation.cancelled().await;
            tokio::task::yield_now().await;
            finished.send(()).unwrap();
        });

        tokio::time::timeout(Duration::from_secs(1), task.shutdown()).await.unwrap();
        cleanup.await.expect("service must finish cleanup rather than be aborted immediately");
    }

    #[tokio::test]
    async fn drop_bounds_uncooperative_startup_and_releases_resources() {
        let (resource, released) = oneshot::channel::<()>();
        let (started, ready) = oneshot::channel();
        let task = ConsensusTask::spawn(CancellationToken::new(), async move {
            let _resource = resource;
            started.send(()).unwrap();
            pending::<()>().await;
        });
        ready.await.unwrap();
        drop(task);

        assert!(
            tokio::time::timeout(ConsensusTask::SHUTDOWN_GRACE + Duration::from_secs(1), released)
                .await
                .expect("uncooperative service must be forcibly dropped")
                .is_err(),
            "service must release its owned resources"
        );
    }

    #[tokio::test]
    async fn shutdown_waits_for_forced_resource_release() {
        let (resource, mut released) = oneshot::channel::<()>();
        let task = ConsensusTask::spawn(CancellationToken::new(), async move {
            let _resource = resource;
            pending::<()>().await;
        });

        tokio::time::timeout(
            ConsensusTask::SHUTDOWN_GRACE + Duration::from_secs(1),
            task.shutdown(),
        )
        .await
        .expect("shutdown must not wait indefinitely for cooperation");
        assert_eq!(released.try_recv(), Err(oneshot::error::TryRecvError::Closed));
    }
}
