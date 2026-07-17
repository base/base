//! High-level load-test execution orchestration.

use std::{
    fmt,
    panic::AssertUnwindSafe,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use alloy_primitives::{U256, utils::format_ether};
use alloy_provider::Provider;
use alloy_signer_local::PrivateKeySigner;
use base_cli_utils::RuntimeManager;
use indicatif::MultiProgress;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    BaselineError, LoadConfig, LoadRunner, LoadTestDisplay, MetricsSummary, RealTokenSetup, Result,
    RpcProviders, RpcResultExt, TestConfig,
};

/// Runtime options for a load-test execution.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadTestRunOptions {
    /// Run continuously until the runner is externally stopped.
    pub continuous: bool,
    /// Install signal handlers that ask the runner to stop gracefully.
    pub install_signal_handler: bool,
}

/// Caller-supplied hooks for a prepared load-test run.
pub struct LoadTestRunHooks<F> {
    /// Optional live progress bars.
    pub display: Option<LoadTestDisplayConfig>,
    /// Invoked after the run completes and before cleanup.
    pub before_cleanup: F,
}

impl<F> fmt::Debug for LoadTestRunHooks<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadTestRunHooks")
            .field("display", &self.display)
            .field("before_cleanup", &"FnOnce")
            .finish()
    }
}

/// Funding and token amounts prepared before a load-test run.
#[derive(Clone, Debug)]
pub struct LoadTestSetupAmounts {
    /// Native ETH funding amount per sender account.
    pub funding: U256,
    /// Fixture swap-token mint/distribution amount.
    pub swap_token: U256,
    /// B-20 mint amount when B-20 setup is required.
    pub b20_mint: U256,
    /// Optional real-token setup replacing fixture token minting.
    pub real_token_setup: Option<RealTokenSetup>,
}

/// Optional live progress display for a load-test run.
#[derive(Debug)]
pub struct LoadTestDisplayConfig {
    /// Indicatif multi-progress handle shared with the tracing writer.
    pub multi_progress: MultiProgress,
    /// Optional run duration used by the progress bar.
    pub duration: Option<Duration>,
}

/// Cleanup work attempted after a load-test run.
#[derive(Clone, Debug, Default)]
pub struct LoadTestCleanupSummary {
    /// Whether B-20 teardown was attempted.
    pub b20_teardown_attempted: bool,
    /// Error returned by B-20 teardown, if any.
    pub b20_teardown_error: Option<String>,
    /// Native ETH drained back to the funder.
    pub drained: Option<U256>,
    /// Error returned by native ETH drain, if any.
    pub drain_error: Option<String>,
}

/// Completed load-test execution.
#[derive(Debug)]
pub struct LoadTestRunOutput {
    /// Metrics summary from the run. If setup or execution failed, `error` is populated.
    pub summary: MetricsSummary,
    /// Cleanup summary.
    pub cleanup: LoadTestCleanupSummary,
    /// Error returned by setup or execution, preserved for callers that need to propagate it.
    pub run_error: Option<BaselineError>,
}

/// Keeps signal-handler tasks alive for a run and aborts them on drop.
#[derive(Debug)]
pub struct SignalHandlerGuard {
    runtime_handle: JoinHandle<()>,
    stop_handle: JoinHandle<()>,
}

impl Drop for SignalHandlerGuard {
    fn drop(&mut self) {
        self.stop_handle.abort();
        self.runtime_handle.abort();
    }
}

const fn noop_before_cleanup(_: &MetricsSummary) {}

/// Executes a configured load test, including setup and cleanup.
#[derive(Debug)]
pub struct LoadTestExecutor;

impl LoadTestExecutor {
    /// Runs a load test from a parsed [`TestConfig`].
    pub async fn run(
        test_config: TestConfig,
        options: LoadTestRunOptions,
    ) -> Result<LoadTestRunOutput> {
        let query_rpc = match test_config.query_rpc.clone() {
            Some(query_rpc) => query_rpc,
            None => test_config.primary_submission_rpc()?.clone(),
        };
        let client = RpcProviders::query(query_rpc)?;
        let rpc_chain_id = if test_config.chain_id.is_none() {
            Some(client.get_chain_id().await.rpc("chain id")?)
        } else {
            None
        };

        let load_config = test_config.to_load_config(rpc_chain_id)?;
        let funding_key = TestConfig::funder_key()?;

        Self::run_prepared(
            test_config,
            load_config,
            funding_key,
            options,
            LoadTestRunHooks { display: None, before_cleanup: noop_before_cleanup },
        )
        .await
    }

    /// Runs a load test whose RPC-derived configuration has already been resolved.
    pub async fn run_prepared<F>(
        test_config: TestConfig,
        load_config: LoadConfig,
        funding_key: PrivateKeySigner,
        options: LoadTestRunOptions,
        hooks: LoadTestRunHooks<F>,
    ) -> Result<LoadTestRunOutput>
    where
        F: FnOnce(&MetricsSummary),
    {
        let load_config =
            if options.continuous { load_config.with_continuous() } else { load_config };
        let setup = LoadTestSetupAmounts {
            funding: test_config.parse_funding_amount()?,
            swap_token: test_config.parse_swap_token_amount()?,
            b20_mint: test_config.parse_b20_mint_amount()?,
            real_token_setup: test_config.parse_real_token_setup(load_config.chain_id)?,
        };
        let config_summary = test_config.to_summary();

        let mut runner = LoadRunner::new(load_config)?;
        runner.set_config_summary(config_summary.clone());
        if let Some(recovery_message) = runner.recovery_message() {
            println!("{recovery_message}");
            println!();
        }
        let _signal_guard = options
            .install_signal_handler
            .then(|| Self::install_signal_handler(runner.stop_flag()));

        let run_result = Self::run_phases(&mut runner, &funding_key, setup, hooks.display).await;
        let (summary, run_error) = match run_result {
            Ok(summary) => (summary, None),
            Err(error) => {
                let summary = MetricsSummary {
                    config: Some(config_summary),
                    error: Some(error.to_string()),
                    ..MetricsSummary::default()
                };
                (summary, Some(error))
            }
        };

        // Cleanup must run even if the caller hook panics, or funded accounts can leak ETH.
        let hook_panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            (hooks.before_cleanup)(&summary);
        }));
        let cleanup = Self::cleanup(&runner, funding_key).await;
        if let Err(payload) = hook_panic {
            std::panic::resume_unwind(payload);
        }

        Ok(LoadTestRunOutput { summary, cleanup, run_error })
    }

    /// Runs txpool clearing, account funding, token setup, and the load loop.
    pub async fn run_phases(
        runner: &mut LoadRunner,
        funding_key: &PrivateKeySigner,
        setup: LoadTestSetupAmounts,
        display: Option<LoadTestDisplayConfig>,
    ) -> Result<MetricsSummary> {
        if runner.txpool_node_count() > 0 {
            println!("Clearing txpool sender transactions...");
            let removed = runner.clear_txpools().await?;
            println!("Txpool clearing complete. Removed {removed} transaction(s).");
        }

        println!("Funding test accounts...");
        runner.fund_accounts(funding_key.clone(), setup.funding).await?;
        println!("Accounts funded.");

        Self::setup_tokens(runner, funding_key, &setup).await?;

        println!();
        println!("Running load test...");
        if let Some(display) = display {
            runner.set_display(LoadTestDisplay::new(&display.multi_progress, display.duration));
        }
        runner.run().await
    }

    /// Prepares optional real-token, swap-token, and B-20 balances.
    pub async fn setup_tokens(
        runner: &mut LoadRunner,
        funding_key: &PrivateKeySigner,
        setup: &LoadTestSetupAmounts,
    ) -> Result<()> {
        if let Some(real_token_setup) = setup.real_token_setup.as_ref() {
            println!("Preparing real-token swap balances...");
            runner.setup_real_tokens(real_token_setup).await?;
            println!("Real-token swap balances prepared.");
        } else if !runner.collect_swap_tokens().is_empty() {
            println!("Distributing swap tokens...");
            runner.setup_swap_tokens(funding_key.clone(), setup.swap_token).await?;
            println!("Swap tokens distributed.");
        }

        if runner.needs_b20_setup() {
            println!("Setting up B-20 tokens...");
            runner.setup_b20_tokens(setup.b20_mint).await?;
            println!("B-20 tokens ready.");
        }

        Ok(())
    }

    /// Burns B-20 balances when needed and drains native ETH back to the funder.
    pub async fn cleanup(
        runner: &LoadRunner,
        funding_key: PrivateKeySigner,
    ) -> LoadTestCleanupSummary {
        tokio::time::sleep(Duration::from_secs(2)).await;

        let mut summary = LoadTestCleanupSummary::default();
        if runner.needs_b20_setup() {
            println!("Burning remaining B-20 tokens...");
            summary.b20_teardown_attempted = true;
            match runner.teardown_b20_tokens().await {
                Ok(()) => println!("B-20 teardown complete."),
                Err(error) => {
                    eprintln!("Warning: B-20 teardown failed: {error}");
                    summary.b20_teardown_error = Some(error.to_string());
                }
            }
        }

        println!();
        println!("Draining accounts back to funder...");
        match runner.drain_accounts(funding_key).await {
            Ok(drained) => {
                println!("Drained {} ETH back to funder.", format_ether(drained));
                summary.drained = Some(drained);
            }
            Err(error) => {
                eprintln!("Warning: drain failed: {error}");
                summary.drain_error = Some(error.to_string());
            }
        }

        summary
    }

    /// Installs SIGINT and SIGTERM handlers that flip the runner stop flag.
    ///
    /// The returned guard must be held for as long as the handlers should remain
    /// active. Dropping it aborts the background tasks so repeated installs do
    /// not accumulate.
    pub fn install_signal_handler(stop_flag: Arc<AtomicBool>) -> SignalHandlerGuard {
        let cancel = CancellationToken::new();
        let runtime_handle = RuntimeManager::install_signal_handler(cancel.clone());

        let stop_handle = tokio::spawn(async move {
            cancel.cancelled().await;
            eprintln!("\nReceived signal, stopping gracefully.");
            stop_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        SignalHandlerGuard { runtime_handle, stop_handle }
    }
}
