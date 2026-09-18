//! Shared lifecycle for isolated, real-client acceptance scenarios.

use std::{
    future::Future,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
};

use base_system_tests::{SystemTestStack, SystemTestStackBuilder};
use eyre::{Result, WrapErr, ensure};
use futures::FutureExt;
use tracing::info;

use super::Rpc;

/// Runs ordinary async scenarios without making them own cleanup and reporting policy.
#[derive(Debug)]
pub struct Acceptance;

impl Acceptance {
    /// Runs a scenario with a caller-selected dedicated-L1 fixture.
    ///
    /// Fixture factories must retain startup logs in the supplied directory. Partial startup
    /// resources remain owned by the existing stack builders and their drop implementations.
    pub async fn run(
        name: &str,
        fixture: impl AsyncFnOnce(&Path) -> Result<SystemTestStackBuilder>,
        scenario: impl AsyncFnOnce(&Rpc, &SystemTestStack) -> Result<()>,
    ) -> Result<()> {
        let parent = std::env::var_os("BASE_ACCEPTANCE_ARTIFACTS")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let artifacts = Self::artifact_directory(&parent, name)?;
        info!(scenario = name, artifacts = %artifacts.display(), "starting acceptance scenario");
        let result = Self::execute(
            async {
                let rpc = Rpc::new()?;
                let system = fixture(&artifacts).await?.build().await?;
                Ok((rpc, system))
            },
            async |state: &(Rpc, SystemTestStack)| scenario(&state.0, &state.1).await,
            async |state: &(Rpc, SystemTestStack)| {
                state.1.capture_diagnostics(&artifacts.join("diagnostics")).await
            },
            async |state: (Rpc, SystemTestStack)| state.1.shutdown().await,
        )
        .await;
        info!(scenario = name, artifacts = %artifacts.display(), success = result.is_ok(), "acceptance scenario finished");
        result.wrap_err_with(|| {
            format!("acceptance scenario {name}; artifacts: {}", artifacts.display())
        })
    }

    /// Creates a persistent, isolated directory whose name identifies the scenario.
    pub fn artifact_directory(parent: &Path, name: &str) -> Result<PathBuf> {
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
            "scenario name must contain only ASCII letters, digits, hyphens, or underscores"
        );
        std::fs::create_dir_all(parent)?;
        Ok(tempfile::Builder::new()
            .prefix(&format!("acceptance-{name}-"))
            .tempdir_in(parent)?
            .keep())
    }

    /// Executes lifecycle stages in order, retaining independent errors and panic messages.
    ///
    /// Shutdown uses the components' own deadlines, not a shorter scenario-level timeout.
    /// Nextest remains the outer process-level deadline and cleanup fallback.
    pub async fn execute<S>(
        setup: impl Future<Output = Result<S>>,
        scenario: impl AsyncFnOnce(&S) -> Result<()>,
        diagnostics: impl AsyncFnOnce(&S) -> Result<()>,
        shutdown: impl AsyncFnOnce(S) -> Result<()>,
    ) -> Result<()> {
        let system = Self::catch("setup", setup).await?;
        let scenario = Self::catch("scenario", async { scenario(&system).await }).await;
        let diagnostics = Self::catch("diagnostics", async { diagnostics(&system).await }).await;
        let shutdown = Self::catch("shutdown", async { shutdown(system).await }).await;
        let failures: Vec<_> = [scenario, diagnostics, shutdown]
            .into_iter()
            .filter_map(Result::err)
            .map(|error| format!("{error:#}"))
            .collect();
        ensure!(failures.is_empty(), "{}", failures.join("\n"));
        Ok(())
    }

    /// Converts a stage's unwind into a report so later cleanup stages can still run.
    pub async fn catch<T>(stage: &str, future: impl Future<Output = Result<T>>) -> Result<T> {
        match AssertUnwindSafe(future).catch_unwind().await {
            Ok(result) => result.wrap_err_with(|| format!("{stage} failed")),
            Err(panic) => {
                let message = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("non-string panic");
                Err(eyre::eyre!("{stage} panicked: {message}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, time::Duration};

    use eyre::eyre;

    use super::Acceptance;

    #[tokio::test]
    async fn retains_all_failures_and_runs_cleanup_in_order() {
        let calls = RefCell::new(Vec::new());
        let result = Acceptance::execute(
            async {
                calls.borrow_mut().push("setup");
                Ok(())
            },
            async |_: &()| {
                calls.borrow_mut().push("scenario");
                Err(eyre!("wrong balance"))
            },
            async |_: &()| {
                calls.borrow_mut().push("diagnostics");
                Err(eyre!("unreadable log"))
            },
            async |()| {
                calls.borrow_mut().push("shutdown");
                Err(eyre!("drain failed"))
            },
        )
        .await
        .unwrap_err();
        assert_eq!(*calls.borrow(), ["setup", "scenario", "diagnostics", "shutdown"]);
        let message = format!("{result:#}");
        for expected in [
            "scenario failed: wrong balance",
            "diagnostics failed: unreadable log",
            "shutdown failed: drain failed",
        ] {
            assert!(message.contains(expected), "{message}");
        }
    }

    #[tokio::test]
    async fn panics_do_not_skip_later_cleanup_stages() {
        let error = Acceptance::execute(
            async { Ok(()) },
            async |_: &()| panic!("scenario panic"),
            async |_: &()| std::panic::panic_any(String::from("diagnostic panic")),
            async |()| Err(eyre!("shutdown still ran")),
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        for expected in [
            "scenario panicked: scenario panic",
            "diagnostics panicked: diagnostic panic",
            "shutdown still ran",
        ] {
            assert!(message.contains(expected), "{message}");
        }
    }

    #[tokio::test]
    async fn setup_failure_does_not_run_stages_without_a_system() {
        let error = Acceptance::execute(
            async { Err::<(), _>(eyre!("L1 unavailable")) },
            async |_: &()| panic!("scenario must not run"),
            async |_: &()| panic!("diagnostics must not run"),
            async |()| panic!("shutdown must not run"),
        )
        .await
        .unwrap_err();
        assert_eq!(format!("{error:#}"), "setup failed: L1 unavailable");
    }

    #[tokio::test]
    async fn startup_failure_reports_and_preserves_fixture_logs() {
        let directory = RefCell::new(None);
        let error = Acceptance::run(
            "startup-failure",
            async |path| {
                *directory.borrow_mut() = Some(path.to_owned());
                std::fs::create_dir_all(path.join("diagnostics"))?;
                std::fs::write(path.join("diagnostics/startup.log"), "client startup failed")?;
                Err(eyre!("fixture unavailable"))
            },
            async |_, _| panic!("scenario must not run"),
        )
        .await
        .unwrap_err();
        let directory = directory.into_inner().unwrap();
        let message = format!("{error:#}");
        assert!(message.contains(directory.to_str().unwrap()), "{message}");
        assert!(message.contains("setup failed: fixture unavailable"), "{message}");
        assert_eq!(
            std::fs::read_to_string(directory.join("diagnostics/startup.log")).unwrap(),
            "client startup failed"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn permits_a_graceful_drain_longer_than_sixty_seconds() {
        let start = tokio::time::Instant::now();
        Acceptance::execute(
            async { Ok(()) },
            async |_: &()| Ok(()),
            async |_: &()| Ok(()),
            async |()| {
                tokio::time::sleep(Duration::from_secs(90)).await;
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(90));
    }

    #[test]
    fn artifacts_are_isolated_persistent_and_named_for_the_scenario() {
        let parent = tempfile::tempdir().unwrap();
        let first = Acceptance::artifact_directory(parent.path(), "blob-safe").unwrap();
        let second = Acceptance::artifact_directory(parent.path(), "blob-safe").unwrap();
        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
        assert!(first.file_name().unwrap().to_str().unwrap().contains("blob-safe"));
        assert!(Acceptance::artifact_directory(parent.path(), "../escape").is_err());
    }
}
