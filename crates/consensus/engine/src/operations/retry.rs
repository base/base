//! Retry helpers for mutating engine operations.

use std::{fmt::Display, future::Future, pin::Pin};

use tokio::time::{Duration, sleep};

use crate::{Engine, EngineState, EngineTaskError, EngineTaskErrorSeverity, Metrics};

const TEMPORARY_ENGINE_ERROR_RETRY_DELAY: Duration = Duration::from_millis(50);

impl Engine {
    /// Retries an engine operation until it succeeds or returns a non-temporary error.
    pub async fn retry_with_severity<Output, Error, Operation>(
        &mut self,
        label: &'static str,
        mut operation: Operation,
    ) -> Result<Output, Error>
    where
        Output: Send,
        Error: Display + EngineTaskError + Send,
        Operation: for<'state> FnMut(
            &'state mut EngineState,
        ) -> Pin<
            Box<dyn Future<Output = Result<Output, Error>> + Send + 'state>,
        >,
    {
        let _task_timer = base_metrics::timed!(Metrics::engine_task_duration(label));

        loop {
            match operation(&mut self.state).await {
                Ok(output) => {
                    self.state_sender.send_replace(self.state);
                    Metrics::engine_task_count(label).increment(1);
                    return Ok(output);
                }
                Err(err) => {
                    self.state_sender.send_replace(self.state);
                    let severity = err.severity();
                    Metrics::engine_task_failure(label, severity.as_label()).increment(1);

                    match severity {
                        EngineTaskErrorSeverity::Temporary => {
                            trace!(target: "engine", error = %err, "Temporary engine error");
                            sleep(TEMPORARY_ENGINE_ERROR_RETRY_DELAY).await;
                        }
                        EngineTaskErrorSeverity::Critical => {
                            error!(target: "engine", error = %err, "Critical engine error");
                            return Err(err);
                        }
                        EngineTaskErrorSeverity::Reset => {
                            warn!(target: "engine", "Engine requested derivation reset");
                            return Err(err);
                        }
                        EngineTaskErrorSeverity::Flush => {
                            warn!(target: "engine", "Engine requested derivation flush");
                            return Err(err);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use tokio::sync::watch;

    use super::*;
    use crate::{EngineSyncStateUpdate, test_utils::test_block_info};

    #[derive(Debug)]
    enum TestError {
        Flush,
    }

    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Flush => f.write_str("flush"),
            }
        }
    }

    impl EngineTaskError for TestError {
        fn severity(&self) -> EngineTaskErrorSeverity {
            match self {
                Self::Flush => EngineTaskErrorSeverity::Flush,
            }
        }
    }

    #[tokio::test]
    async fn non_temporary_error_publishes_mutated_state_before_returning() {
        let initial_state = EngineState::default();
        let (state_tx, state_rx) = watch::channel(initial_state);
        let mut engine = Engine::new(initial_state, state_tx);
        let new_head = test_block_info(42);

        let result: Result<(), TestError> = engine
            .retry_with_severity("test", move |state| {
                Box::pin(async move {
                    state.sync_state = state.sync_state.apply_update(EngineSyncStateUpdate {
                        unsafe_head: Some(new_head),
                        ..Default::default()
                    });
                    Err(TestError::Flush)
                })
            })
            .await;

        assert!(matches!(result, Err(TestError::Flush)));
        assert_eq!(state_rx.borrow().sync_state.unsafe_head(), new_head);
    }
}
