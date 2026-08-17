//! Utilities for the rollup node service, internal to the crate.

use tracing::info;

/// Spawns a set of parallel actors in a [`JoinSet`], and cancels all actors if any of them fail. The
/// type of the error in the [`NodeActor`]s is erased to avoid having to specify a common error type
/// between actors.
///
/// Actors are passed in as optional arguments, in case a given actor is not needed.
///
/// Two actor groups are supported:
/// - `actors = [...]` — **fatal** actors. Each installs a [`drop_guard`], so if the actor returns
///   (`Ok` or `Err`) or panics, the shared cancellation token is cancelled and the whole node shuts
///   down. This is the fail-stop behavior every existing actor relies on (op-conductor sequencer,
///   engine, derivation, network, …).
/// - `non_fatal = [...]` — **non-fatal** actors (optional group). These install **no** drop guard and
///   their errors are logged and swallowed, so an actor in this group completing or erroring does
///   **not** cancel the shared token or bring down the node. Use this only for experimental /
///   isolated actors that must have zero blast radius on the live path (e.g. the simplex consensus
///   actor in non-authoritative modes). Both groups share one [`JoinSet`], so shutdown-signal
///   handling and abort-on-drop are identical.
///
/// This macro also handles OS shutdown signals (SIGTERM, SIGINT) and triggers graceful shutdown
/// when received.
///
/// [JoinSet]: tokio::task::JoinSet
/// [NodeActor]: crate::NodeActor
/// [drop_guard]: tokio_util::sync::CancellationToken::drop_guard
macro_rules! spawn_and_wait {
    // Public entry: fatal actors only — forwards to the full form with an empty non-fatal group so
    // the shutdown/await loop is defined exactly once.
    ($cancellation:expr, actors = [$($actor:expr$(,)?)*]) => {
        $crate::service::spawn_and_wait!($cancellation, actors = [$($actor,)*], non_fatal = []);
    };
    // Public entry: fatal + non-fatal actors.
    (
        $cancellation:expr,
        actors = [$($actor:expr$(,)?)*],
        non_fatal = [$($non_fatal_actor:expr$(,)?)*]
    ) => {
        use tracing::{error, info};
        let mut task_handles = tokio::task::JoinSet::new();

        // Fatal actors: spawn with a drop guard so any return/panic cancels the node.
        $(
            if let Some((actor, context)) = $actor {
                let cancellation = $cancellation.clone();
                task_handles.spawn(async move {
                    // This guard ensures that the cancellation token is cancelled when the actor is
                    // dropped. This ensures that the actor is properly shut down.
                    // Note the underscore prefix: this is to signal that we don't use the guard anywhere, but
                    // *the compiler shouldn't optimize it away*.
                    // Note that using a simple `_` would not work here because it gets optimized away in
                    // release mode.
                    let _guard = cancellation.drop_guard();

                    if let Err(e) = actor.start(context).await {
                        return Err(format!("{e:?}"));
                    }
                    Ok(())
                });
            }
        )*

        // Non-fatal actors: NO drop guard; errors AND panics are caught, logged, and swallowed to
        // `Ok(())`. All three are load-bearing for zero blast radius: (1) no drop_guard, so a normal
        // return doesn't cancel the token; (2) `Err` swallowed, so the `Some(Ok(Err(_)))` fatal arm
        // of the await loop can't fire; (3) `catch_unwind`, so a panic doesn't surface as a
        // `JoinError` and hit the `Some(Err(_))` fatal arm. Without (3) a panicking non-fatal actor
        // would still take the whole node (incl. the live op-conductor path) down.
        $(
            if let Some((actor, context)) = $non_fatal_actor {
                task_handles.spawn(async move {
                    use futures::FutureExt as _;
                    match std::panic::AssertUnwindSafe(actor.start(context)).catch_unwind().await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            error!(target: "rollup_node", error = ?e, "Non-fatal actor error; ignoring to preserve node isolation");
                        }
                        Err(_) => {
                            error!(target: "rollup_node", "Non-fatal actor panicked; ignoring to preserve node isolation");
                        }
                    }
                    Ok::<(), String>(())
                });
            }
        )*

        // Create the shutdown signal future
        let shutdown = $crate::ShutdownSignal::wait();
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    info!(target: "rollup_node", "Received shutdown signal, initiating graceful shutdown...");
                    $cancellation.cancel();
                    break;
                }
                result = task_handles.join_next() => {
                    match result {
                        Some(Ok(Ok(()))) => { /* Actor completed successfully */ }
                        Some(Ok(Err(e))) => {
                            error!(target: "rollup_node", error = %e, "Critical error in sub-routine");
                            // Cancel all tasks and gracefully shutdown.
                            $cancellation.cancel();
                            return Err(e);
                        }
                        Some(Err(e)) => {
                            let error_msg = format!("Task join error: {e}");
                            // Log the error and cancel all tasks.
                            error!(target: "rollup_node", error = %e, "Task join error");
                            // Cancel all tasks and gracefully shutdown.
                            $cancellation.cancel();
                            return Err(error_msg);
                        }
                        None => break, // All tasks completed
                    }
                }
            }
        }
    };
}

// Export the `spawn_and_wait` macro for use in other modules.
pub(crate) use spawn_and_wait;

/// Listens for OS shutdown signals (SIGTERM, SIGINT)
#[derive(Debug)]
pub struct ShutdownSignal;

impl ShutdownSignal {
    /// Waits for OS shutdown signals (SIGTERM, SIGINT).
    pub async fn wait() {
        let ctrl_c = async {
            tokio::signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
        };

        #[cfg(unix)]
        let terminate = async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {
                info!(target: "rollup_node", "Received SIGINT (Ctrl+C)");
            },
            _ = terminate => {
                info!(target: "rollup_node", "Received SIGTERM");
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use crate::NodeActor;

    /// Test actor that returns immediately with the configured result.
    #[derive(Debug)]
    struct FinishActor(Result<(), String>);

    #[async_trait]
    impl NodeActor for FinishActor {
        type Error = String;
        type StartData = ();

        async fn start(self, _: Self::StartData) -> Result<(), Self::Error> {
            self.0
        }
    }

    /// Test actor that panics, to exercise the non-fatal `catch_unwind`.
    #[derive(Debug)]
    struct PanicActor;

    #[async_trait]
    impl NodeActor for PanicActor {
        type Error = String;
        type StartData = ();

        async fn start(self, _: Self::StartData) -> Result<(), Self::Error> {
            panic!("boom");
        }
    }

    /// A non-fatal actor that returns `Err` must NOT cancel the shared token or
    /// make the node return an error — the whole point of the `non_fatal` slot.
    async fn run_non_fatal_err(cancellation: CancellationToken) -> Result<(), String> {
        spawn_and_wait!(
            cancellation,
            actors = [],
            non_fatal = [Some((FinishActor(Err("boom".to_string())), ()))]
        );
        Ok(())
    }

    #[tokio::test]
    async fn non_fatal_err_does_not_cancel_token() {
        let cancellation = CancellationToken::new();
        assert!(run_non_fatal_err(cancellation.clone()).await.is_ok());
        assert!(!cancellation.is_cancelled(), "non-fatal Err must not cancel the shared token");
    }

    /// A non-fatal actor that PANICS must also not cancel the token (caught via
    /// `catch_unwind`); without the guard the panic would surface as a `JoinError`
    /// and trip the fatal arm.
    async fn run_non_fatal_panic(cancellation: CancellationToken) -> Result<(), String> {
        spawn_and_wait!(cancellation, actors = [], non_fatal = [Some((PanicActor, ()))]);
        Ok(())
    }

    #[tokio::test]
    async fn non_fatal_panic_does_not_cancel_token() {
        let cancellation = CancellationToken::new();
        assert!(run_non_fatal_panic(cancellation.clone()).await.is_ok());
        assert!(!cancellation.is_cancelled(), "non-fatal panic must not cancel the shared token");
    }

    /// A fatal actor returning `Err` cancels the token and returns the error —
    /// the existing fail-stop behavior every live actor relies on, unchanged.
    async fn run_fatal_err(cancellation: CancellationToken) -> Result<(), String> {
        spawn_and_wait!(cancellation, actors = [Some((FinishActor(Err("boom".to_string())), ()))]);
        Ok(())
    }

    #[tokio::test]
    async fn fatal_err_cancels_token_and_returns_err() {
        let cancellation = CancellationToken::new();
        let result = run_fatal_err(cancellation.clone()).await;
        assert!(result.is_err(), "fatal Err must propagate");
        assert!(cancellation.is_cancelled(), "fatal Err must cancel the shared token");
    }

    /// The fatal-only forwarding arm (no `non_fatal` group) must expand and run —
    /// a fatal actor returning `Ok` still cancels the token via its drop guard.
    async fn run_fatal_ok_via_forwarding_arm(
        cancellation: CancellationToken,
    ) -> Result<(), String> {
        spawn_and_wait!(cancellation, actors = [Some((FinishActor(Ok(())), ()))]);
        Ok(())
    }

    #[tokio::test]
    async fn fatal_ok_via_forwarding_arm_cancels_token() {
        let cancellation = CancellationToken::new();
        assert!(run_fatal_ok_via_forwarding_arm(cancellation.clone()).await.is_ok());
        assert!(cancellation.is_cancelled(), "fatal actor return fires the drop guard");
    }
}
