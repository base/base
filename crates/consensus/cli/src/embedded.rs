//! Coordinated shutdown for consensus embedded alongside execution.

use std::future::Future;

use base_common_genesis::RollupConfig;
use base_upgrade_signal::UpgradeSignalStartupMode;
use reth_tasks::TaskExecutor;
use tokio_util::sync::CancellationToken;

use crate::{ConsensusNodeArgs, ConsensusNodeOverrides, ConsensusNodeStartOptions};

impl ConsensusNodeArgs {
    /// Starts embedded consensus after upgrade-signal startup has already been applied.
    ///
    /// Keeps the execution node handle alive until both nodes complete coordinated shutdown.
    pub async fn start_with_execution<N, E>(
        &self,
        rollup_config: RollupConfig,
        overrides: ConsensusNodeOverrides,
        execution_node: N,
        execution_exit: E,
        task_executor: TaskExecutor,
    ) -> eyre::Result<()>
    where
        E: Future<Output = eyre::Result<()>>,
    {
        let cancellation = CancellationToken::new();
        let consensus_exit = self.start_with_options(
            ConsensusNodeStartOptions::new(rollup_config)
                .with_overrides(overrides)
                .with_cancellation(cancellation.clone())
                .with_upgrade_signal_startup_mode(UpgradeSignalStartupMode::AlreadyApplied),
        );
        let result = Self::wait_for_execution_and_consensus(
            execution_exit,
            consensus_exit,
            cancellation,
            || async {
                task_executor
                    .initiate_graceful_shutdown()
                    .map_err(|e| eyre::eyre!("failed to signal execution node shutdown: {e}"))?
                    .ignore_guard()
                    .await;
                Ok(())
            },
        )
        .await;
        drop(execution_node);
        result
    }

    /// Stops and awaits the other node when either exits, preserving the first exit error.
    ///
    /// Execution exit cancels consensus. Consensus exit invokes the execution shutdown
    /// callback. A failure to signal execution shutdown is returned immediately.
    pub async fn wait_for_execution_and_consensus<E, C, S, F>(
        execution_exit: E,
        consensus_exit: C,
        consensus_cancellation: CancellationToken,
        shutdown_execution: S,
    ) -> eyre::Result<()>
    where
        E: Future<Output = eyre::Result<()>>,
        C: Future<Output = eyre::Result<()>>,
        S: FnOnce() -> F,
        F: Future<Output = eyre::Result<()>>,
    {
        tokio::pin!(execution_exit);
        tokio::pin!(consensus_exit);

        tokio::select! {
            result = &mut execution_exit => {
                consensus_cancellation.cancel();
                let consensus_result = consensus_exit.await;
                result?;
                consensus_result
            }
            result = &mut consensus_exit => {
                shutdown_execution().await?;
                let execution_result = execution_exit.await;
                result?;
                execution_result
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, time::Duration};

    use tokio::{sync::oneshot, time::timeout};
    use tokio_util::sync::CancellationToken;

    use crate::ConsensusNodeArgs;

    #[tokio::test]
    async fn stops_and_awaits_sibling_preserving_error_precedence() {
        for execution_first in [true, false] {
            for first_fails in [true, false] {
                for sibling_fails in [true, false] {
                    let cancellation = CancellationToken::new();
                    let sibling_finished = Cell::new(false);
                    let (shutdown_tx, shutdown_rx) = oneshot::channel();
                    let execution_exit = async {
                        if !execution_first {
                            shutdown_rx.await.unwrap();
                            sibling_finished.set(true);
                        }
                        if (execution_first && first_fails) || (!execution_first && sibling_fails) {
                            eyre::bail!("execution failed");
                        }
                        Ok(())
                    };
                    let consensus_exit = async {
                        if execution_first {
                            cancellation.cancelled().await;
                            sibling_finished.set(true);
                        }
                        if (!execution_first && first_fails) || (execution_first && sibling_fails) {
                            eyre::bail!("consensus failed");
                        }
                        Ok(())
                    };
                    let result = timeout(
                        Duration::from_secs(5),
                        ConsensusNodeArgs::wait_for_execution_and_consensus(
                            execution_exit,
                            consensus_exit,
                            cancellation.clone(),
                            || async {
                                shutdown_tx.send(()).unwrap();
                                Ok(())
                            },
                        ),
                    )
                    .await
                    .expect("coordinated shutdown should complete");

                    assert!(sibling_finished.get());
                    assert_eq!(cancellation.is_cancelled(), execution_first);
                    if first_fails || sibling_fails {
                        let execution_error =
                            if first_fails { execution_first } else { !execution_first };
                        assert_eq!(
                            result.unwrap_err().to_string(),
                            if execution_error { "execution failed" } else { "consensus failed" }
                        );
                    } else {
                        result.unwrap();
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn propagates_execution_shutdown_signal_failure() {
        let result = timeout(
            Duration::from_secs(5),
            ConsensusNodeArgs::wait_for_execution_and_consensus(
                std::future::pending(),
                async { Ok(()) },
                CancellationToken::new(),
                || async { eyre::bail!("shutdown signal failed") },
            ),
        )
        .await
        .expect("shutdown signal failure should return immediately");

        assert_eq!(result.unwrap_err().to_string(), "shutdown signal failed");
    }
}
